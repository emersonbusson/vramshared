// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-block/src/origin_cache.rs
//! Durable origin with a revocable, best-effort VRAM block cache.
use std::cmp;
use std::fs::File;
use std::io;
#[cfg(unix)]
use std::os::unix::fs::FileExt;
#[cfg(windows)]
use std::os::windows::fs::FileExt;
use std::path::Path;
use std::time::{Duration, Instant};

use ramshared_vram::{RUNTIME_FREE_BUFFER_BYTES, VramMemory, VramProvider};

use crate::{BlockBackend, IoError, WriteOptions};

pub const GIB: u64 = 1024 * 1024 * 1024;
pub const ORIGIN_CACHE_CHUNK_BYTES: u64 = 128 * 1024 * 1024;
const HEALTHY_SAMPLES_TO_GROW: u8 = 3;
const RESTRICTED_SAMPLES_TO_RECLAIM: u8 = 3;
const GROWTH_INTERVAL: Duration = Duration::from_secs(2);
const STUCK_AFTER: Duration = Duration::from_secs(2);

/// Positional origin I/O takes `&self`: the durable device is addressed by
/// offset, so one origin can serve concurrent requests (SPEC
/// `wsl2-nbd-serving-throughput` DT-3).
pub trait OriginStorage {
    fn read_at(&self, off: u64, buf: &mut [u8]) -> Result<usize, IoError>;
    fn write_at(&self, off: u64, data: &[u8]) -> Result<usize, IoError>;
    fn sync_data(&self) -> Result<(), IoError>;

    fn read_exact_at(&self, mut off: u64, mut buf: &mut [u8]) -> Result<(), IoError> {
        while !buf.is_empty() {
            let read = self.read_at(off, buf)?;
            if read == 0 {
                return Err(IoError("origin read made no progress".into()));
            }
            if read > buf.len() {
                return Err(IoError("origin read exceeded requested length".into()));
            }
            off = off
                .checked_add(read as u64)
                .ok_or_else(|| IoError("origin read offset overflow".into()))?;
            buf = &mut buf[read..];
        }
        Ok(())
    }

    fn write_all_at(&self, mut off: u64, mut data: &[u8]) -> Result<(), IoError> {
        while !data.is_empty() {
            let written = self.write_at(off, data)?;
            if written == 0 {
                return Err(IoError("origin write made no progress".into()));
            }
            if written > data.len() {
                return Err(IoError("origin write exceeded requested length".into()));
            }
            off = off
                .checked_add(written as u64)
                .ok_or_else(|| IoError("origin write offset overflow".into()))?;
            data = &data[written..];
        }
        Ok(())
    }
}

pub struct FileOrigin {
    file: File,
}
impl FileOrigin {
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        Ok(Self {
            file: File::options().read(true).write(true).open(path)?,
        })
    }
    pub fn from_file(file: File) -> Self {
        Self { file }
    }
}
impl OriginStorage for FileOrigin {
    fn read_at(&self, off: u64, buf: &mut [u8]) -> Result<usize, IoError> {
        #[cfg(unix)]
        {
            self.file
                .read_at(buf, off)
                .map_err(|error| IoError(error.to_string()))
        }
        #[cfg(windows)]
        {
            self.file
                .seek_read(buf, off)
                .map_err(|error| IoError(error.to_string()))
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = (off, buf);
            Err(IoError("unsupported platform for FileOrigin".into()))
        }
    }

    fn write_at(&self, off: u64, data: &[u8]) -> Result<usize, IoError> {
        #[cfg(unix)]
        {
            self.file
                .write_at(data, off)
                .map_err(|error| IoError(error.to_string()))
        }
        #[cfg(windows)]
        {
            self.file
                .seek_write(data, off)
                .map_err(|error| IoError(error.to_string()))
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = (off, data);
            Err(IoError("unsupported platform for FileOrigin".into()))
        }
    }

    fn sync_data(&self) -> Result<(), IoError> {
        self.file
            .sync_data()
            .map_err(|error| IoError(error.to_string()))
    }
}

mod policy;

pub use policy::{
    CachePolicyOutcome, CacheState, CacheTelemetry, GpuSample, OriginCacheIo, OriginState,
    SAMPLE_MAX_AGE, physical_target_bytes,
};

struct CacheChunk<M> {
    mem: Option<M>,
    generation: u64,
    validity_generation: u64,
    valid: Vec<bool>,
    last_access: u64,
}

pub struct WriteThroughCacheBackend<'p, P: VramProvider + 'p, O> {
    provider: &'p P,
    origin: O,
    size: u64,
    block: u32,
    chunk_bytes: u64,
    chunks: Vec<CacheChunk<P::Mem<'p>>>,
    telemetry: CacheTelemetry,
    target_bytes: u64,
    physical_cap_bytes: u64,
    /// Configured reserve floor component (DT-2). Supplied by the sealed
    /// policy at startup; `0` means "no configured floor", so the 20%
    /// capacity share still binds inside `safe_target_bytes`.
    configured_reserve_bytes: u64,
    /// Runtime free buffer applied after the reserve (DT-4).
    runtime_headroom_bytes: u64,
    healthy_samples: u8,
    restricted_samples: u8,
    last_growth_at: Option<Duration>,
    over_target_since: Option<Duration>,
    access_clock: u64,
    origin_state: OriginState,
    origin_probe_successes: u8,
    origin_dirty: bool,
    cache_state: CacheState,
}

impl<'p, P: VramProvider + 'p, O: OriginStorage> WriteThroughCacheBackend<'p, P, O> {
    pub fn new(provider: &'p P, origin: O, size: u64, block: u32) -> Result<Self, IoError> {
        Self::with_chunk_bytes(provider, origin, size, block, ORIGIN_CACHE_CHUNK_BYTES)
    }

    #[doc(hidden)]
    pub fn with_chunk_bytes(
        provider: &'p P,
        origin: O,
        size: u64,
        block: u32,
        chunk_bytes: u64,
    ) -> Result<Self, IoError> {
        if size == 0
            || block == 0
            || chunk_bytes == 0
            || !size.is_multiple_of(block as u64)
            || !chunk_bytes.is_multiple_of(block as u64)
        {
            return Err(IoError("invalid origin cache geometry".into()));
        }
        let chunk_count = size.div_ceil(chunk_bytes);
        let mut chunks = Vec::with_capacity(chunk_count as usize);
        for index in 0..chunk_count {
            let start = index * chunk_bytes;
            let len = cmp::min(chunk_bytes, size - start);
            chunks.push(CacheChunk {
                mem: None,
                generation: 1,
                validity_generation: 0,
                valid: vec![false; (len / block as u64) as usize],
                last_access: 0,
            });
        }
        Ok(Self {
            provider,
            origin,
            size,
            block,
            chunk_bytes,
            chunks,
            telemetry: CacheTelemetry::default(),
            target_bytes: 0,
            physical_cap_bytes: size,
            configured_reserve_bytes: 0,
            runtime_headroom_bytes: RUNTIME_FREE_BUFFER_BYTES,
            healthy_samples: 0,
            restricted_samples: 0,
            last_growth_at: None,
            over_target_since: None,
            access_clock: 0,
            origin_state: OriginState::Ready,
            origin_probe_successes: 0,
            origin_dirty: false,
            cache_state: CacheState::Off,
        })
    }

    pub fn chunk_bytes(&self) -> u64 {
        self.chunk_bytes
    }

    pub fn cached_bytes(&self) -> u64 {
        self.chunks.iter().fold(0, |total, chunk| {
            total + chunk.mem.as_ref().map_or(0, |_| self.chunk_bytes)
        })
    }

    pub fn target_bytes(&self) -> u64 {
        self.target_bytes
    }

    pub fn set_physical_cap_bytes(&mut self, cap_bytes: u64) {
        self.physical_cap_bytes = cap_bytes.min(self.size);
    }

    /// Binds the sealed reserve floor this cache must respect (DT-2/DT-4).
    ///
    /// Called once at startup with the resolved policy. The runtime headroom
    /// is taken from the caller so one authority supplies it.
    pub fn set_reserve_floor(
        &mut self,
        configured_reserve_bytes: u64,
        runtime_headroom_bytes: u64,
    ) {
        self.configured_reserve_bytes = configured_reserve_bytes;
        self.runtime_headroom_bytes = runtime_headroom_bytes;
    }

    pub fn configured_reserve_bytes(&self) -> u64 {
        self.configured_reserve_bytes
    }

    pub fn runtime_headroom_bytes(&self) -> u64 {
        self.runtime_headroom_bytes
    }

    pub fn origin_state(&self) -> OriginState {
        self.origin_state
    }

    pub fn cache_state(&self) -> CacheState {
        self.cache_state
    }

    pub fn telemetry(&self) -> CacheTelemetry {
        CacheTelemetry {
            valid_blocks: self.valid_block_count(),
            ..self.telemetry
        }
    }

    pub fn release_cache(&mut self) -> u64 {
        let released = self.release_lru_to(0);
        self.cache_state = CacheState::Off;
        released
    }

    /// Forwards the sample and the growth tick. Does **not** validate and does
    /// **not** stamp provenance (DT-6): [`physical_target_bytes`] owns both.
    pub fn observe_gpu(
        &mut self,
        sample: Option<&GpuSample>,
        tick: Duration,
        now: Instant,
    ) -> CachePolicyOutcome {
        let target = physical_target_bytes(
            self.physical_cap_bytes,
            sample,
            self.configured_reserve_bytes,
            self.runtime_headroom_bytes,
            now,
        );
        self.target_bytes = target;
        let cached_before = self.cached_bytes();
        let missing = sample.is_none();
        let restricted = missing || target < cached_before || target < self.chunk_bytes;

        if restricted {
            self.healthy_samples = 0;
            self.restricted_samples = self.restricted_samples.saturating_add(1);
            self.cache_state = if missing {
                CacheState::Unavailable
            } else {
                CacheState::Restricted
            };
        } else {
            self.restricted_samples = 0;
            self.healthy_samples = self.healthy_samples.saturating_add(1);
            self.cache_state = CacheState::Active;
        }

        let mut released_bytes = 0;
        if restricted && self.restricted_samples >= RESTRICTED_SAMPLES_TO_RECLAIM {
            released_bytes = self.release_lru_to(target);
            self.restricted_samples = 0;
        }

        let mut allocated_bytes = 0;
        if !restricted
            && self.healthy_samples >= HEALTHY_SAMPLES_TO_GROW
            && self.cached_bytes().saturating_add(self.chunk_bytes) <= target
            && self
                .last_growth_at
                .is_none_or(|previous| tick.saturating_sub(previous) >= GROWTH_INTERVAL)
        {
            self.last_growth_at = Some(tick);
            match self.allocate_one_chunk() {
                Ok(bytes) => allocated_bytes = bytes,
                Err(()) => self.cache_state = CacheState::Unavailable,
            }
        }

        let excess = self.cached_bytes().saturating_sub(target);
        if excess > self.chunk_bytes {
            let since = self.over_target_since.get_or_insert(tick);
            if tick.saturating_sub(*since) > STUCK_AFTER {
                self.cache_state = CacheState::Stuck;
            }
        } else {
            self.over_target_since = None;
        }

        CachePolicyOutcome {
            target_bytes: target,
            allocated_bytes,
            released_bytes,
        }
    }

    pub fn probe_origin(&mut self) -> Result<OriginState, IoError> {
        let mut block = vec![0; self.block as usize];
        let result = self
            .origin
            .read_exact_at(0, &mut block)
            .and_then(|()| self.origin.sync_data());
        if let Err(error) = result {
            self.mark_origin_failed();
            return Err(error);
        }
        self.origin_dirty = false;
        self.telemetry.origin_syncs = self.telemetry.origin_syncs.saturating_add(1);
        if matches!(
            self.origin_state,
            OriginState::Failed | OriginState::Degraded
        ) {
            self.origin_probe_successes = self.origin_probe_successes.saturating_add(1);
            if self.origin_probe_successes >= 3 {
                self.origin_state = OriginState::Ready;
                self.origin_probe_successes = 0;
            } else {
                self.origin_state = OriginState::Degraded;
            }
        }
        Ok(self.origin_state)
    }

    fn check_range(&self, off: u64, len: usize) -> Result<(), IoError> {
        off.checked_add(len as u64)
            .filter(|end| *end <= self.size)
            .map(|_| ())
            .ok_or_else(|| IoError("origin cache I/O is out of range".into()))
    }

    fn valid_block_count(&self) -> u64 {
        self.chunks
            .iter()
            .map(|chunk| {
                chunk
                    .valid
                    .iter()
                    .filter(|valid| {
                        **valid
                            && chunk.validity_generation == chunk.generation
                            && chunk.mem.is_some()
                    })
                    .count() as u64
            })
            .sum()
    }

    fn allocate_one_chunk(&mut self) -> Result<u64, ()> {
        let Some(index) = self.chunks.iter().position(|chunk| chunk.mem.is_none()) else {
            return Ok(0);
        };
        let bytes = self.chunk_bytes as usize;
        let mut mem = match self.provider.alloc(bytes) {
            Ok(mem) => mem,
            Err(_) => {
                self.telemetry.allocation_failures =
                    self.telemetry.allocation_failures.saturating_add(1);
                return Err(());
            }
        };
        if mem.zero().is_err() {
            self.telemetry.allocation_failures =
                self.telemetry.allocation_failures.saturating_add(1);
            return Err(());
        }
        self.access_clock = self.access_clock.saturating_add(1);
        self.chunks[index].last_access = self.access_clock;
        self.chunks[index].validity_generation = self.chunks[index].generation;
        self.chunks[index].valid.fill(false);
        self.chunks[index].mem = Some(mem);
        Ok(self.chunk_bytes)
    }

    fn release_lru_to(&mut self, target: u64) -> u64 {
        let mut released = 0u64;
        while self.cached_bytes() > target {
            let Some((index, _)) = self
                .chunks
                .iter()
                .enumerate()
                .filter(|(_, chunk)| chunk.mem.is_some())
                .min_by_key(|(_, chunk)| chunk.last_access)
            else {
                break;
            };
            self.invalidate_chunk(index);
            self.chunks[index].mem = None;
            released = released.saturating_add(self.chunk_bytes);
            self.telemetry.releases = self.telemetry.releases.saturating_add(1);
        }
        released
    }

    fn invalidate_chunk(&mut self, index: usize) {
        let chunk = &mut self.chunks[index];
        chunk.generation = chunk.generation.wrapping_add(1).max(1);
        chunk.valid.fill(false);
        self.telemetry.invalidations = self.telemetry.invalidations.saturating_add(1);
    }

    /// A failed origin write may have made partial progress. Invalidate every
    /// overlapping clean cache chunk before origin recovery can permit reads.
    fn invalidate_cached_range(&mut self, off: u64, len: usize) {
        if len == 0 {
            return;
        }
        let first = (off / self.chunk_bytes) as usize;
        let last = ((off + len as u64 - 1) / self.chunk_bytes) as usize;
        for index in first..=last {
            if self.chunks[index].mem.is_some() {
                self.invalidate_chunk(index);
            }
        }
    }

    fn invalidate_all_cached(&mut self) {
        for index in 0..self.chunks.len() {
            if self.chunks[index].mem.is_some() {
                self.invalidate_chunk(index);
            }
        }
    }

    fn mark_origin_failed(&mut self) {
        self.origin_state = OriginState::Failed;
        self.origin_probe_successes = 0;
    }

    fn require_ready_origin(&self) -> Result<(), IoError> {
        if self.origin_state == OriginState::Ready {
            Ok(())
        } else {
            Err(IoError(
                "origin authority is unavailable pending three read+sync probes".into(),
            ))
        }
    }

    fn range_is_cached(&self, off: u64, len: usize) -> bool {
        let mut done = 0usize;
        while done < len {
            let absolute = off + done as u64;
            let index = (absolute / self.chunk_bytes) as usize;
            let relative = absolute % self.chunk_bytes;
            let count = (len - done).min((self.chunk_bytes - relative) as usize);
            let chunk = &self.chunks[index];
            if chunk.mem.is_none() {
                return false;
            }
            let first_block = relative / self.block as u64;
            let last_block = (relative + count as u64).div_ceil(self.block as u64);
            if chunk.validity_generation != chunk.generation
                || (first_block..last_block).any(|block| !chunk.valid[block as usize])
            {
                return false;
            }
            done += count;
        }
        true
    }

    fn cache_read(&mut self, off: u64, buf: &mut [u8]) -> Result<(), usize> {
        let mut done = 0usize;
        while done < buf.len() {
            let absolute = off + done as u64;
            let index = (absolute / self.chunk_bytes) as usize;
            let relative = absolute % self.chunk_bytes;
            let count = (buf.len() - done).min((self.chunk_bytes - relative) as usize);
            let result = self.chunks[index]
                .mem
                .as_ref()
                .ok_or(index)
                .and_then(|mem| {
                    mem.read_at(relative, &mut buf[done..done + count])
                        .map_err(|_| index)
                });
            result?;
            self.access_clock = self.access_clock.saturating_add(1);
            self.chunks[index].last_access = self.access_clock;
            done += count;
        }
        Ok(())
    }

    fn update_cached_chunks(&mut self, off: u64, data: &[u8], count_refusals: bool) {
        let mut done = 0usize;
        while done < data.len() {
            let absolute = off + done as u64;
            let index = (absolute / self.chunk_bytes) as usize;
            let relative = absolute % self.chunk_bytes;
            let count = (data.len() - done).min((self.chunk_bytes - relative) as usize);
            let Some(mem) = self.chunks[index].mem.as_mut() else {
                if count_refusals {
                    self.telemetry.promotion_refusals =
                        self.telemetry.promotion_refusals.saturating_add(1);
                }
                done += count;
                continue;
            };
            if mem.write_at(relative, &data[done..done + count]).is_err() {
                self.telemetry.cache_write_failures =
                    self.telemetry.cache_write_failures.saturating_add(1);
                self.invalidate_chunk(index);
                done += count;
                continue;
            }
            self.access_clock = self.access_clock.saturating_add(1);
            self.chunks[index].last_access = self.access_clock;
            self.mark_fully_covered_blocks(index, relative, count);
            done += count;
        }
    }

    fn mark_fully_covered_blocks(&mut self, index: usize, relative: u64, len: usize) {
        let block = self.block as u64;
        let first = relative.div_ceil(block);
        let last = (relative + len as u64) / block;
        let chunk = &mut self.chunks[index];
        if chunk.validity_generation != chunk.generation {
            chunk.valid.fill(false);
            chunk.validity_generation = chunk.generation;
        }
        for block_index in first..last {
            if let Some(valid) = chunk.valid.get_mut(block_index as usize) {
                *valid = true;
            }
        }
    }

    fn write_origin(&mut self, off: u64, data: &[u8]) -> Result<(), IoError> {
        if let Err(error) = self.origin.write_all_at(off, data) {
            self.invalidate_cached_range(off, data.len());
            self.mark_origin_failed();
            return Err(error);
        }
        self.origin_dirty = true;
        self.telemetry.origin_written_bytes = self
            .telemetry
            .origin_written_bytes
            .saturating_add(data.len() as u64);
        Ok(())
    }

    fn sync_dirty_origin(&mut self) -> Result<(), IoError> {
        if !self.origin_dirty {
            return Ok(());
        }
        if let Err(error) = self.origin.sync_data() {
            self.invalidate_all_cached();
            self.mark_origin_failed();
            return Err(error);
        }
        self.origin_dirty = false;
        self.telemetry.origin_syncs = self.telemetry.origin_syncs.saturating_add(1);
        Ok(())
    }
}

impl<P: VramProvider, O: OriginStorage> BlockBackend for WriteThroughCacheBackend<'_, P, O> {
    fn size_bytes(&self) -> u64 {
        self.size
    }

    fn block_size(&self) -> u32 {
        self.block
    }

    fn read_at(&mut self, off: u64, buf: &mut [u8]) -> Result<(), IoError> {
        self.check_range(off, buf.len())?;
        self.require_ready_origin()?;
        if buf.is_empty() {
            return Ok(());
        }
        if self.range_is_cached(off, buf.len()) {
            match self.cache_read(off, buf) {
                Ok(()) => {
                    self.telemetry.cache_read_bytes = self
                        .telemetry
                        .cache_read_bytes
                        .saturating_add(buf.len() as u64);
                    self.telemetry.cache_hits = self.telemetry.cache_hits.saturating_add(1);
                    return Ok(());
                }
                Err(index) => {
                    self.telemetry.cache_read_failures =
                        self.telemetry.cache_read_failures.saturating_add(1);
                    self.invalidate_chunk(index);
                }
            }
        }
        if let Err(error) = self.origin.read_exact_at(off, buf) {
            self.mark_origin_failed();
            return Err(error);
        }
        self.telemetry.fallback_reads = self.telemetry.fallback_reads.saturating_add(1);
        if self.cached_bytes() <= self.target_bytes {
            self.update_cached_chunks(off, buf, true);
        } else {
            self.telemetry.promotion_refusals = self.telemetry.promotion_refusals.saturating_add(1);
        }
        Ok(())
    }

    fn write_at(&mut self, off: u64, data: &[u8]) -> Result<(), IoError> {
        self.check_range(off, data.len())?;
        self.require_ready_origin()?;
        if data.is_empty() {
            return Ok(());
        }
        self.write_origin(off, data)?;
        self.telemetry.batched_writes = self.telemetry.batched_writes.saturating_add(1);
        self.update_cached_chunks(off, data, false);
        Ok(())
    }

    fn write_at_with_options(
        &mut self,
        off: u64,
        data: &[u8],
        options: WriteOptions,
    ) -> Result<(), IoError> {
        if !options.fua {
            return self.write_at(off, data);
        }
        self.check_range(off, data.len())?;
        self.require_ready_origin()?;
        if data.is_empty() {
            return Ok(());
        }
        self.write_origin(off, data)?;
        self.sync_dirty_origin()?;
        self.update_cached_chunks(off, data, false);
        Ok(())
    }

    fn flush(&mut self) -> Result<(), IoError> {
        self.require_ready_origin()?;
        self.sync_dirty_origin()
    }
}

#[cfg(test)]
mod tests;
