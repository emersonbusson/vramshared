// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-block/src/gpu_cache_worker/tests/codec_fixtures.rs
#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::super::*;
use super::raw_fixtures::*;
use super::*;

/// A provider that reports the deterministic fake codec (DT-2).
///
/// `free` is shared so a test can publish under headroom and then drop the
/// live free level to model an external GPU workload.
pub(super) struct CodecProvider {
    pub(super) total: u64,
    pub(super) free: Arc<AtomicU64>,
    pub(super) live_allocations: Arc<AtomicUsize>,
    pub(super) codec: FakeCodec,
}

impl CodecProvider {
    pub(super) fn new(total: u64, free: u64) -> Self {
        Self {
            total,
            free: Arc::new(AtomicU64::new(free)),
            live_allocations: Arc::new(AtomicUsize::new(0)),
            codec: FakeCodec::new(),
        }
    }

    pub(super) fn set_free(&self, free: u64) {
        self.free.store(free, Ordering::SeqCst);
    }
}

impl VramProvider for CodecProvider {
    type Mem<'p>
        = FakeMem
    where
        Self: 'p;

    fn alloc(&self, bytes: usize) -> Result<Self::Mem<'_>, VramError> {
        self.live_allocations.fetch_add(1, Ordering::SeqCst);
        Ok(FakeMem {
            data: Arc::new(Mutex::new(vec![0u8; bytes])),
            len: bytes,
            live_allocations: Arc::clone(&self.live_allocations),
        })
    }

    fn mem_info(&self) -> Result<(u64, u64), VramError> {
        Ok((self.free.load(Ordering::SeqCst), self.total))
    }

    fn budget_snapshot(&self) -> Result<GpuBudgetSnapshot, VramError> {
        Ok(trusted_test_budget(
            self.free.load(Ordering::SeqCst),
            self.total,
        ))
    }

    fn cache_codec(&self) -> Option<&dyn GpuCacheCodec<Self::Mem<'_>>> {
        Some(&self.codec)
    }
}

/// A provider whose budget snapshot is stale (DT-4 admission refusal).
pub(super) struct StaleBudgetProvider {
    pub(super) total: u64,
    pub(super) free: u64,
    pub(super) codec: FakeCodec,
}

impl VramProvider for StaleBudgetProvider {
    type Mem<'p>
        = FakeMem
    where
        Self: 'p;

    fn alloc(&self, bytes: usize) -> Result<Self::Mem<'_>, VramError> {
        Ok(FakeMem {
            data: Arc::new(Mutex::new(vec![0u8; bytes])),
            len: bytes,
            live_allocations: Arc::new(AtomicUsize::new(1)),
        })
    }

    fn mem_info(&self) -> Result<(u64, u64), VramError> {
        Ok((self.free, self.total))
    }

    fn budget_snapshot(&self) -> Result<GpuBudgetSnapshot, VramError> {
        Ok(GpuBudgetSnapshot {
            adapter: Some(GpuAdapterIdentity {
                node_mask: None,
                backend: "test".into(),
                key: "stale-adapter-0".into(),
                luid: None,
            }),
            total_bytes: Some(self.total),
            budget_bytes: self.total,
            used_bytes: self.total.saturating_sub(self.free),
            source: GpuBudgetSource::DriverReported,
            // Far enough in the past that `can_admit` refuses.
            sampled_at: Instant::now() - Duration::from_secs(60),
        })
    }

    fn cache_codec(&self) -> Option<&dyn GpuCacheCodec<Self::Mem<'_>>> {
        Some(&self.codec)
    }
}

/// A provider with a codec but no measured budget at all (zero target).
pub(super) struct NoBudgetProvider {
    pub(super) codec: FakeCodec,
}

impl VramProvider for NoBudgetProvider {
    type Mem<'p>
        = FakeMem
    where
        Self: 'p;

    fn alloc(&self, bytes: usize) -> Result<Self::Mem<'_>, VramError> {
        Ok(FakeMem {
            data: Arc::new(Mutex::new(vec![0u8; bytes])),
            len: bytes,
            live_allocations: Arc::new(AtomicUsize::new(1)),
        })
    }

    fn mem_info(&self) -> Result<(u64, u64), VramError> {
        Err(VramError::Provider("no budget".into()))
    }

    fn cache_codec(&self) -> Option<&dyn GpuCacheCodec<Self::Mem<'_>>> {
        Some(&self.codec)
    }
}

/// A provider whose codec fails every operation (DT-11 fault injection).
pub(super) struct FaultyCodec;

impl GpuCacheCodec<FakeMem> for FaultyCodec {
    fn codec_id(&self) -> CodecId {
        CodecId::Fake
    }
    fn required_alignments(&self) -> ramshared_vram::CodecAlignments {
        ramshared_vram::CodecAlignments::byte()
    }
    fn max_encoded_len(&self, _logical_len: usize) -> Result<usize, VramError> {
        Err(VramError::Provider("codec unavailable".into()))
    }
    fn workspace_bytes(
        &self,
        _item_count: usize,
        _max_logical_len: usize,
    ) -> Result<usize, VramError> {
        Ok(0)
    }
    fn compress_batch_into(
        &self,
        _inputs: &[&[u8]],
        _slab: &mut FakeMem,
        _outputs: &[ramshared_vram::VramOutputReservation],
        _workspace: &mut FakeMem,
    ) -> Result<Vec<ramshared_vram::CodecChunkResult>, VramError> {
        Err(VramError::Provider("codec fault".into()))
    }
    fn checksum_batch(
        &self,
        _slab: &FakeMem,
        _inputs: &[ramshared_vram::VramSpan],
        _workspace: &mut FakeMem,
    ) -> Result<Vec<u32>, VramError> {
        Err(VramError::Provider("codec fault".into()))
    }
    fn decompress_batch_from(
        &self,
        _slab: &FakeMem,
        _inputs: &[ramshared_vram::VramSpan],
        _logical_lengths: &[usize],
        _outputs: &mut [Vec<u8>],
        _workspace: &mut FakeMem,
    ) -> Result<Vec<CodecStatus>, VramError> {
        Err(VramError::Provider("codec fault".into()))
    }
}

pub(super) struct FaultyProvider {
    pub(super) total: u64,
    pub(super) free: u64,
    pub(super) live_allocations: Arc<AtomicUsize>,
    pub(super) codec: FaultyCodec,
}

impl VramProvider for FaultyProvider {
    type Mem<'p>
        = FakeMem
    where
        Self: 'p;

    fn alloc(&self, bytes: usize) -> Result<Self::Mem<'_>, VramError> {
        self.live_allocations.fetch_add(1, Ordering::SeqCst);
        Ok(FakeMem {
            data: Arc::new(Mutex::new(vec![0u8; bytes])),
            len: bytes,
            live_allocations: Arc::clone(&self.live_allocations),
        })
    }

    fn mem_info(&self) -> Result<(u64, u64), VramError> {
        Ok((self.free, self.total))
    }

    fn budget_snapshot(&self) -> Result<GpuBudgetSnapshot, VramError> {
        Ok(trusted_test_budget(self.free, self.total))
    }

    fn cache_codec(&self) -> Option<&dyn GpuCacheCodec<Self::Mem<'_>>> {
        Some(&self.codec)
    }
}

#[derive(Clone, Copy)]
pub(super) enum ScriptedCodecFailure {
    None,
    FailCompress,
    FailPublishChecksum,
    FailDecodeChecksum,
    FailDecompress,
    BadDecompressStatus,
    WrongDecompressLen,
    CorruptDecompressBytes,
    DelayDecompress,
}

pub(super) struct ScriptedCodec {
    pub(super) codec_id: CodecId,
    pub(super) failure: std::cell::Cell<ScriptedCodecFailure>,
    pub(super) inner: FakeCodec,
}

impl ScriptedCodec {
    pub(super) fn new(codec_id: CodecId, failure: ScriptedCodecFailure) -> Self {
        Self {
            codec_id,
            failure: std::cell::Cell::new(failure),
            inner: FakeCodec::new(),
        }
    }
}

// `GpuCacheCodec` methods take `&self`; `ScriptedCodec` is used strictly in
// single-threaded tests (`--test-threads=1`).
impl GpuCacheCodec<FakeMem> for ScriptedCodec {
    fn codec_id(&self) -> CodecId {
        self.codec_id
    }

    fn required_alignments(&self) -> ramshared_vram::CodecAlignments {
        <FakeCodec as GpuCacheCodec<FakeMem>>::required_alignments(&self.inner)
    }

    fn max_encoded_len(&self, logical_len: usize) -> Result<usize, VramError> {
        <FakeCodec as GpuCacheCodec<FakeMem>>::max_encoded_len(&self.inner, logical_len)
    }

    fn workspace_bytes(
        &self,
        item_count: usize,
        max_logical_len: usize,
    ) -> Result<usize, VramError> {
        <FakeCodec as GpuCacheCodec<FakeMem>>::workspace_bytes(
            &self.inner,
            item_count,
            max_logical_len,
        )
    }

    fn compress_batch_into(
        &self,
        inputs: &[&[u8]],
        slab: &mut FakeMem,
        outputs: &[ramshared_vram::VramOutputReservation],
        workspace: &mut FakeMem,
    ) -> Result<Vec<ramshared_vram::CodecChunkResult>, VramError> {
        if matches!(self.failure.get(), ScriptedCodecFailure::FailCompress) {
            return Err(VramError::Provider("injected compress failure".into()));
        }
        self.inner
            .compress_batch_into(inputs, slab, outputs, workspace)
    }

    fn checksum_batch(
        &self,
        slab: &FakeMem,
        inputs: &[ramshared_vram::VramSpan],
        workspace: &mut FakeMem,
    ) -> Result<Vec<u32>, VramError> {
        if matches!(
            self.failure.get(),
            ScriptedCodecFailure::FailPublishChecksum | ScriptedCodecFailure::FailDecodeChecksum
        ) {
            return Err(VramError::Provider("injected checksum failure".into()));
        }
        self.inner.checksum_batch(slab, inputs, workspace)
    }

    fn decompress_batch_from(
        &self,
        slab: &FakeMem,
        inputs: &[ramshared_vram::VramSpan],
        logical_lengths: &[usize],
        outputs: &mut [Vec<u8>],
        workspace: &mut FakeMem,
    ) -> Result<Vec<CodecStatus>, VramError> {
        match self.failure.get() {
            ScriptedCodecFailure::DelayDecompress => {
                std::thread::sleep(
                    super::super::compressed_codec::CODEC_SUBDEADLINE + Duration::from_millis(10),
                );
                self.inner
                    .decompress_batch_from(slab, inputs, logical_lengths, outputs, workspace)
            }
            ScriptedCodecFailure::FailDecompress => {
                Err(VramError::Provider("injected decompress failure".into()))
            }
            ScriptedCodecFailure::BadDecompressStatus => Ok(vec![CodecStatus::Failed]),
            ScriptedCodecFailure::WrongDecompressLen => {
                outputs[0] = vec![0u8; 7];
                Ok(vec![CodecStatus::Ok])
            }
            ScriptedCodecFailure::CorruptDecompressBytes => {
                let statuses = self.inner.decompress_batch_from(
                    slab,
                    inputs,
                    logical_lengths,
                    outputs,
                    workspace,
                )?;
                if let Some(first) = outputs[0].first_mut() {
                    *first ^= 0xff;
                }
                Ok(statuses)
            }
            _ => {
                self.inner
                    .decompress_batch_from(slab, inputs, logical_lengths, outputs, workspace)
            }
        }
    }
}

pub(super) struct ScriptedCodecProvider {
    pub(super) total: u64,
    pub(super) free: u64,
    pub(super) live_allocations: Arc<AtomicUsize>,
    pub(super) codec: ScriptedCodec,
}

impl ScriptedCodecProvider {
    pub(super) fn new(
        total: u64,
        free: u64,
        codec_id: CodecId,
        failure: ScriptedCodecFailure,
    ) -> Self {
        Self {
            total,
            free,
            live_allocations: Arc::new(AtomicUsize::new(0)),
            codec: ScriptedCodec::new(codec_id, failure),
        }
    }
}

impl VramProvider for ScriptedCodecProvider {
    type Mem<'p>
        = FakeMem
    where
        Self: 'p;

    fn alloc(&self, bytes: usize) -> Result<Self::Mem<'_>, VramError> {
        self.live_allocations.fetch_add(1, Ordering::SeqCst);
        Ok(FakeMem {
            data: Arc::new(Mutex::new(vec![0u8; bytes])),
            len: bytes,
            live_allocations: Arc::clone(&self.live_allocations),
        })
    }

    fn mem_info(&self) -> Result<(u64, u64), VramError> {
        Ok((self.free, self.total))
    }

    fn budget_snapshot(&self) -> Result<GpuBudgetSnapshot, VramError> {
        Ok(trusted_test_budget(self.free, self.total))
    }

    fn cache_codec(&self) -> Option<&dyn GpuCacheCodec<Self::Mem<'_>>> {
        Some(&self.codec)
    }
}
