// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-vulkan/src/compute_lz4_codec/tests.rs
#![allow(clippy::unwrap_used)]
use super::*;
use ramshared_vram::codec::crc32;
use ramshared_vram::slab_pool::{SlabClass, SlotEncoding};

fn compressible_page(seed: u8) -> [u8; PAGE_BYTES] {
    let mut page = [0u8; PAGE_BYTES];
    for (index, byte) in page.iter_mut().enumerate() {
        *byte = if index % 64 < 32 {
            seed
        } else {
            (index / 64) as u8
        };
    }
    page
}

#[test]
fn vulkan_compute_lz4_roundtrip_and_crc32_gate() {
    let mut codec = VulkanComputeLz4Codec::new();
    let mut pool = VramSlabPool::new(4).unwrap();
    let pages = [compressible_page(1), compressible_page(2)];
    let mut tickets = [None; 2];
    codec
        .compress_and_pack_batch(&pages, &mut pool, &mut tickets)
        .unwrap();
    assert!(tickets[0].unwrap().vram_byte_offset().unwrap() % MIN_STORAGE_ALIGNMENT == 0);
    assert_eq!(tickets[0].unwrap().encoding, SlotEncoding::Lz4);
    let mut decoded = [[0u8; PAGE_BYTES]; 2];
    let mut status = [PageDecodeStatus::Ok; 2];
    codec
        .verify_and_unpack_batch(
            &[tickets[0].unwrap(), tickets[1].unwrap()],
            &mut decoded,
            &mut status,
        )
        .unwrap();
    assert_eq!(status, [PageDecodeStatus::Ok, PageDecodeStatus::Ok]);
    assert_eq!(decoded[0], pages[0]);
    assert_eq!(decoded[1], pages[1]);
    assert!(codec.hardware_spirv_embedded());
    assert!(validate_embedded_spirv_words(EMBEDDED_LZ4_CRC32_SPIRV));
    assert!(!validate_embedded_spirv_words(
        &EMBEDDED_LZ4_CRC32_SPIRV[..8]
    ));
    let mut corrupt = EMBEDDED_LZ4_CRC32_SPIRV.to_vec();
    corrupt[1] = 0x0002_0000;
    assert!(!validate_embedded_spirv_words(&corrupt));
}

#[test]
fn vulkan_codec_default_and_fence_counter_are_total() {
    let mut codec = VulkanComputeLz4Codec::default();
    assert_eq!(codec.backend_name(), "vulkan-compute");
    assert_eq!(codec.fences_polled(), 0);
    let mut pool = VramSlabPool::new(1).unwrap();
    let mut tickets = [None; 1];
    codec
        .compress_and_pack_batch(&[compressible_page(1)], &mut pool, &mut tickets)
        .unwrap();
    assert!(codec.fences_polled() >= 1);
}

#[test]
fn vulkan_compute_malformed_lz4_token_rejected_in_bounds() {
    let mut codec = VulkanComputeLz4Codec::new();
    let mut pool = VramSlabPool::new(1).unwrap();
    let mut tickets = [None; 1];
    codec
        .compress_and_pack_batch(&[compressible_page(8)], &mut pool, &mut tickets)
        .unwrap();
    let mut ticket = tickets[0].unwrap();
    let crc = {
        let payload = codec.reference_payload_mut(&ticket).unwrap();
        let bytes = [0x1F, 0, 0, 1, 2, 3];
        let take = payload.len().min(bytes.len());
        payload[..take].copy_from_slice(&bytes[..take]);
        crc32(payload, 0)
    };
    ticket.payload_crc32 = crc;
    let mut decoded = [[0u8; PAGE_BYTES]; 1];
    let mut status = [PageDecodeStatus::Ok; 1];
    codec
        .verify_and_unpack_batch(&[ticket], &mut decoded, &mut status)
        .unwrap();
    assert_eq!(status[0], PageDecodeStatus::CorruptLz4Stream);
    assert!(decoded[0].iter().all(|byte| *byte == 0));
}

#[test]
fn vulkan_compute_fence_timeout_trips_breaker_cleanly() {
    let mut codec = VulkanComputeLz4Codec::new();
    codec.set_force_timeout(true);
    let mut pool = VramSlabPool::new(1).unwrap();
    let mut tickets = [None; 1];
    let error = codec
        .compress_and_pack_batch(&[compressible_page(1)], &mut pool, &mut tickets)
        .unwrap_err();
    assert_eq!(error, CodecError::DeadlineExceeded);
    assert!(codec.poisoned());
    assert_eq!(pool.logical_pages_cached(), 0);
    let error = codec
        .verify_and_unpack_batch(&[], &mut [], &mut [])
        .unwrap_err();
    assert_eq!(error, CodecError::DeadlineExceeded);
}

#[test]
fn vulkan_compute_raw_fallback_stays_aligned() {
    let mut codec = VulkanComputeLz4Codec::new();
    let mut pool = VramSlabPool::new(1).unwrap();
    let mut page = [0u8; PAGE_BYTES];
    let mut state = 0xA5A5_5A5Au32;
    for byte in &mut page {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        *byte = state as u8;
    }
    let mut tickets = [None; 1];
    codec
        .compress_and_pack_batch(&[page], &mut pool, &mut tickets)
        .unwrap();
    assert_eq!(tickets[0].unwrap().class, SlabClass::Raw4096);
    assert_eq!(tickets[0].unwrap().encoding, SlotEncoding::Raw);
}

#[test]
fn embedded_spirv_words_pass_structural_validator() {
    assert!(validate_embedded_spirv_words(EMBEDDED_LZ4_CRC32_SPIRV));
    for take in [0usize, 4, 8, EMBEDDED_LZ4_CRC32_SPIRV.len() - 1] {
        assert!(!validate_embedded_spirv_words(
            &EMBEDDED_LZ4_CRC32_SPIRV[..take]
        ));
    }
    let mut wrong_model = EMBEDDED_LZ4_CRC32_SPIRV.to_vec();
    wrong_model[1] = 0x0001_0300;
    assert!(!validate_embedded_spirv_words(&wrong_model));

    let mut zero_bound = EMBEDDED_LZ4_CRC32_SPIRV.to_vec();
    zero_bound[3] = 0;
    assert!(!validate_embedded_spirv_words(&zero_bound));

    let mut nonzero_schema = EMBEDDED_LZ4_CRC32_SPIRV.to_vec();
    nonzero_schema[4] = 1;
    assert!(!validate_embedded_spirv_words(&nonzero_schema));

    let mut zero_word_count = EMBEDDED_LZ4_CRC32_SPIRV.to_vec();
    zero_word_count[5] &= 0x0000_FFFF;
    assert!(!validate_embedded_spirv_words(&zero_word_count));

    let mut overflow_word_count = EMBEDDED_LZ4_CRC32_SPIRV.to_vec();
    overflow_word_count[5] = (0xFFFF << 16) | 17;
    assert!(!validate_embedded_spirv_words(&overflow_word_count));

    let mut codec = VulkanComputeLz4Codec::new();
    codec.set_deadline(std::time::Duration::from_secs(1));
}

#[test]
#[ignore = "explicit Vulkan pipeline API test; requires a loader and a selected ICD"]
fn live_vulkan_spirv_shader_compile_and_compute_dispatch() {
    let count = crate::VulkanProvider::device_count()
        .unwrap_or_else(|error| panic!("live Vulkan prerequisite: {error}"));
    if std::env::var_os("RAMSHARED_TEST_VULKAN_MASK_REQUIRED").is_some() {
        assert_eq!(
            count, 0,
            "Vulkan test isolation failed: mask still exposes devices; refusing allocations"
        );
    }
    assert!(count > 0, "live Vulkan prerequisite: no available devices");
    let provider = crate::VulkanProvider::open(0).unwrap();
    let mut codec = VulkanComputeLz4Codec::new();
    let before_fences = codec.fences_polled();
    codec.execute_spirv_compute_pipeline(&provider).unwrap();
    assert!(codec.fences_polled() > before_fences);
    eprintln!(
        "VULKAN_PIPELINE_API_DISPATCH_OK device={:?}",
        provider.device_name()
    );
}

#[cfg(target_os = "linux")]
#[test]
fn vulkan_live_dispatch_refuses_unavailable_icd() {
    let executable = std::env::current_exe().unwrap();
    let native_case =
        "compute_lz4_codec::tests::live_vulkan_spirv_shader_compile_and_compute_dispatch";
    let listing = std::process::Command::new(&executable)
        .args([
            native_case,
            "--exact",
            "--ignored",
            "--list",
            "--color",
            "never",
        ])
        .output()
        .unwrap();
    assert!(listing.status.success());
    assert!(
        String::from_utf8_lossy(&listing.stdout).contains(&format!("{native_case}: test")),
        "native case must require explicit opt-in"
    );
    let missing = executable.with_file_name(format!(
        "ramshared-no-icd-{}-{}.json",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    assert!(!missing.exists());
    let output = std::process::Command::new(&executable)
        .args([
            native_case,
            "--exact",
            "--include-ignored",
            "--nocapture",
            "--color",
            "never",
        ])
        .env("RAMSHARED_TEST_VULKAN_MASK_REQUIRED", "1")
        .env("VK_DRIVER_FILES", &missing)
        .env("VK_ICD_FILENAMES", &missing)
        .env_remove("VK_ADD_DRIVER_FILES")
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !output.status.success(),
        "unavailable backend was falsely qualified: {text}"
    );
    assert!(
        text.contains("live Vulkan prerequisite:"),
        "unexpected failure: {text}"
    );
    assert!(!text.contains("VULKAN_PIPELINE_API_DISPATCH_OK"));
    assert!(
        text.contains("test result: FAILED. 0 passed; 1 failed"),
        "the native case must actually execute and fail: {text}"
    );
}

#[derive(Default)]
struct FakeVulkanCodecDevice {
    slab: Vec<u8>,
    workspace_bytes: usize,
    fail_write_after: Option<usize>,
    writes: usize,
    fail_read: bool,
    fail_dispatch: bool,
    dispatches: u64,
    released: std::rc::Rc<std::cell::Cell<bool>>,
}

impl FakeVulkanCodecDevice {
    fn new(pool_bytes: usize) -> (Self, std::rc::Rc<std::cell::Cell<bool>>) {
        let released = std::rc::Rc::new(std::cell::Cell::new(false));
        (
            Self {
                slab: vec![0u8; pool_bytes],
                workspace_bytes: device_ops::VULKAN_WORKSPACE_BYTES as usize,
                fail_write_after: None,
                writes: 0,
                fail_read: false,
                fail_dispatch: false,
                dispatches: 0,
                released: released.clone(),
            },
            released,
        )
    }
}

impl device_ops::VulkanCodecDevice for FakeVulkanCodecDevice {
    fn pool_bytes(&self) -> usize {
        self.slab.len()
    }

    fn workspace_bytes(&self) -> usize {
        self.workspace_bytes
    }

    fn write_slab(&mut self, offset: u64, src: &[u8]) -> Result<(), CodecError> {
        if self
            .fail_write_after
            .is_some_and(|limit| self.writes >= limit)
        {
            return Err(CodecError::BackendFailure("injected write failure".into()));
        }
        self.writes += 1;
        let off = offset as usize;
        let end = off
            .checked_add(src.len())
            .ok_or_else(|| CodecError::BackendFailure("offset overflow in fake slab".into()))?;
        if end > self.slab.len() {
            return Err(CodecError::BackendFailure("out of bounds write".into()));
        }
        self.slab[off..end].copy_from_slice(src);
        Ok(())
    }

    fn read_slab(&self, offset: u64, dst: &mut [u8]) -> Result<(), CodecError> {
        if self.fail_read {
            return Err(CodecError::BackendFailure("injected read failure".into()));
        }
        let off = offset as usize;
        let end = off
            .checked_add(dst.len())
            .ok_or_else(|| CodecError::BackendFailure("offset overflow in fake slab".into()))?;
        if end > self.slab.len() {
            return Err(CodecError::BackendFailure("out of bounds read".into()));
        }
        dst.copy_from_slice(&self.slab[off..end]);
        Ok(())
    }

    fn dispatch_compute(&mut self, _workgroups_x: u32) -> Result<(), CodecError> {
        if self.fail_dispatch {
            return Err(CodecError::BackendFailure(
                "injected dispatch failure".into(),
            ));
        }
        self.dispatches += 1;
        Ok(())
    }

    fn release_buffers(&mut self) {
        self.released.set(true);
    }
}

#[test]
fn vulkan_device_codec_core_roundtrip_tamper_rollback_and_poison_retention() {
    use device_ops::{
        VULKAN_RESERVE_FLOOR_EXTRA_BYTES, VULKAN_WORKSPACE_BYTES, VulkanDeviceArena,
        VulkanDeviceCodecCore, validate_vulkan_pool_budget,
    };
    use ramshared_vram::slab_pool::FRAME_BYTES;

    // Budget validation refuses zero frames, tiny budgets, and oversized requests.
    assert!(validate_vulkan_pool_budget(16 * 1024 * 1024, 0).is_err());
    assert!(validate_vulkan_pool_budget(VULKAN_RESERVE_FLOOR_EXTRA_BYTES - 1, 1).is_err());
    assert!(validate_vulkan_pool_budget(VULKAN_RESERVE_FLOOR_EXTRA_BYTES + 65536, 100).is_err());
    let (arena, pool_bytes) = validate_vulkan_pool_budget(
        VULKAN_RESERVE_FLOOR_EXTRA_BYTES + (4 * FRAME_BYTES as u64),
        4,
    )
    .unwrap();
    assert_eq!(pool_bytes, 4 * FRAME_BYTES);
    assert!(!arena.retained());

    // Normal roundtrip: compressible LZ4 + incompressible Raw4096 + bitflip CRC tamper + invalidation + teardown.
    let (fake, released) = FakeVulkanCodecDevice::new(pool_bytes);
    let mut codec = VulkanDeviceCodecCore::new(arena, fake);
    assert_eq!(codec.backend_name(), "vulkan-compute");
    assert_eq!(
        codec.device_allocation_bytes(),
        Some((pool_bytes as u64, VULKAN_WORKSPACE_BYTES))
    );
    assert!(!codec.poisoned());
    assert!(!codec.arena().retained());

    let mut pool = VramSlabPool::new(4).unwrap();
    let mut raw_page = [0u8; PAGE_BYTES];
    let mut x = 0x1234_5678u32;
    for b in &mut raw_page {
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        *b = x as u8;
    }
    let pages = [compressible_page(3), raw_page];
    let mut tickets = [None; 2];
    codec
        .compress_and_pack_batch(&pages, &mut pool, &mut tickets)
        .unwrap();
    let t0 = tickets[0].unwrap();
    let t1 = tickets[1].unwrap();
    assert_eq!(t0.encoding, SlotEncoding::Lz4);
    assert_eq!(t1.encoding, SlotEncoding::Raw);
    assert_eq!(codec.fences_polled(), 1);

    // Empty batches succeed without dispatch; invalid batch lengths refuse.
    codec
        .compress_and_pack_batch(&[], &mut pool, &mut [])
        .unwrap();
    codec
        .verify_and_unpack_batch(&[], &mut [], &mut [])
        .unwrap();
    assert_eq!(
        codec.compress_and_pack_batch(&pages, &mut pool, &mut [None; 1]),
        Err(CodecError::InvalidBatchLength)
    );
    assert_eq!(
        codec.verify_and_unpack_batch(
            &[t0, t1],
            &mut [[0u8; PAGE_BYTES]; 1],
            &mut [PageDecodeStatus::Ok; 2]
        ),
        Err(CodecError::InvalidBatchLength)
    );

    let mut decoded = [[0u8; PAGE_BYTES]; 2];
    let mut status = [PageDecodeStatus::InvalidTicket; 2];
    codec
        .verify_and_unpack_batch(&[t0, t1], &mut decoded, &mut status)
        .unwrap();
    assert_eq!(status, [PageDecodeStatus::Ok, PageDecodeStatus::Ok]);
    assert_eq!(decoded[0], pages[0]);
    assert_eq!(decoded[1], pages[1]);

    // Device bit-flip triggers PreDecodeCrcMismatch and zeroes output.
    codec.inject_vram_bitflip(&t0, 0, 0xFF);
    let mut tamper_out = [[0xAAu8; PAGE_BYTES]; 1];
    let mut tamper_status = [PageDecodeStatus::Ok; 1];
    codec
        .verify_and_unpack_batch(&[t0], &mut tamper_out, &mut tamper_status)
        .unwrap();
    assert_eq!(tamper_status[0], PageDecodeStatus::PreDecodeCrcMismatch);
    assert!(tamper_out[0].iter().all(|b| *b == 0));

    // Invalidation turns subsequent reads into InvalidTicket.
    codec.invalidate(&t0);
    let _ = pool.invalidate(&t0);
    codec
        .verify_and_unpack_batch(&[t0], &mut tamper_out, &mut tamper_status)
        .unwrap();
    assert_eq!(tamper_status[0], PageDecodeStatus::InvalidTicket);

    // Normal drop releases device buffers cleanly.
    drop(codec);
    assert!(released.get());

    // Mid-batch write failure rolls back prior committed tickets cleanly.
    let (mut fake_fail, _) = FakeVulkanCodecDevice::new(pool_bytes);
    fake_fail.fail_write_after = Some(1);
    let mut fail_codec = VulkanDeviceCodecCore::new(arena, fake_fail);
    let mut pool2 = VramSlabPool::new(4).unwrap();
    let mut tickets2 = [None; 2];
    assert!(
        fail_codec
            .compress_and_pack_batch(&pages, &mut pool2, &mut tickets2)
            .is_err()
    );
    assert!(tickets2.iter().all(Option::is_none));
    assert_eq!(pool2.logical_pages_cached(), 0);

    // Dispatch failure also rolls back committed tickets.
    let (mut fake_disp, _) = FakeVulkanCodecDevice::new(pool_bytes);
    fake_disp.fail_dispatch = true;
    let mut disp_codec = VulkanDeviceCodecCore::new(arena, fake_disp);
    assert!(
        disp_codec
            .compress_and_pack_batch(&pages, &mut pool2, &mut tickets2)
            .is_err()
    );
    assert_eq!(pool2.logical_pages_cached(), 0);

    // Timeout poisons codec and retains buffers on drop (DT-3).
    let (fake_poison, released_poison) = FakeVulkanCodecDevice::new(pool_bytes);
    let mut poison_codec = VulkanDeviceCodecCore::new(arena, fake_poison);
    poison_codec.set_deadline(std::time::Duration::from_secs(1));
    poison_codec.set_force_timeout(true);
    assert_eq!(
        poison_codec.compress_and_pack_batch(&pages, &mut pool2, &mut tickets2),
        Err(CodecError::DeadlineExceeded)
    );
    assert!(poison_codec.poisoned());
    assert!(poison_codec.arena().retained());
    assert_eq!(
        poison_codec.verify_and_unpack_batch(&[t1], &mut tamper_out, &mut tamper_status),
        Err(CodecError::DeadlineExceeded)
    );
    drop(poison_codec);
    assert!(
        !released_poison.get(),
        "poisoned Vulkan codec must retain buffers on drop (DT-3)"
    );

    // Undersized device slab buffer refuses out-of-range slab write without leak.
    let (tiny_fake, _) = FakeVulkanCodecDevice::new(64);
    let tiny_arena =
        VulkanDeviceArena::preaccount(VULKAN_RESERVE_FLOOR_EXTRA_BYTES + 65536).unwrap();
    let mut tiny_codec = VulkanDeviceCodecCore::new(tiny_arena, tiny_fake);
    assert!(
        tiny_codec
            .compress_and_pack_batch(&[compressible_page(9)], &mut pool2, &mut [None; 1])
            .is_err()
    );
    assert_eq!(pool2.logical_pages_cached(), 0);
}

#[test]
#[ignore = "explicit live Vulkan device-resident codec test; requires a loader and a selected ICD"]
fn live_vulkan_device_resident_lz4_roundtrip_tamper_and_teardown() {
    use ramshared_vram::VramProvider as _;
    use ramshared_vram::slab_pool::FRAME_BYTES;

    let provider = crate::VulkanProvider::open(0).unwrap();
    let pool_frames = 8u32;
    let avail = VULKAN_RESERVE_FLOOR_EXTRA_BYTES + u64::from(pool_frames) * FRAME_BYTES as u64;
    let mut codec =
        VulkanComputeLz4Codec::new_provider_codec(&provider, avail, pool_frames).unwrap();
    assert_eq!(codec.backend_name(), "vulkan-compute");
    assert_eq!(
        codec.device_allocation_bytes(),
        Some((
            u64::from(pool_frames) * FRAME_BYTES as u64,
            VULKAN_WORKSPACE_BYTES
        ))
    );
    assert!(
        provider.budget_snapshot().unwrap().used_bytes
            >= u64::from(pool_frames) * FRAME_BYTES as u64
    );

    let mut pool = VramSlabPool::new(pool_frames).unwrap();
    let pages = [
        compressible_page(11),
        compressible_page(22),
        compressible_page(33),
    ];
    let mut tickets = [None; 3];
    codec
        .compress_and_pack_batch(&pages, &mut pool, &mut tickets)
        .unwrap();
    let live_tickets: Vec<VramSlabTicket> = tickets.iter().map(|t| t.unwrap()).collect();
    assert!(live_tickets.iter().all(|t| t.encoding == SlotEncoding::Lz4));

    let mut decoded = [[0u8; PAGE_BYTES]; 3];
    let mut status = [PageDecodeStatus::InvalidTicket; 3];
    codec
        .verify_and_unpack_batch(&live_tickets, &mut decoded, &mut status)
        .unwrap();
    assert_eq!(status, [PageDecodeStatus::Ok; 3]);
    assert_eq!(decoded, pages);

    // Flip a byte directly in DEVICE_LOCAL Vulkan memory -> PreDecodeCrcMismatch.
    codec.inject_vram_bitflip(&live_tickets[1], 0, 0x01);
    let mut after_flip = [[0x55u8; PAGE_BYTES]; 3];
    let mut flip_status = [PageDecodeStatus::Ok; 3];
    codec
        .verify_and_unpack_batch(&live_tickets, &mut after_flip, &mut flip_status)
        .unwrap();
    assert_eq!(flip_status[0], PageDecodeStatus::Ok);
    assert_eq!(flip_status[1], PageDecodeStatus::PreDecodeCrcMismatch);
    assert!(after_flip[1].iter().all(|b| *b == 0));
    assert_eq!(flip_status[2], PageDecodeStatus::Ok);

    // Pressure teardown drops DEVICE_LOCAL slab + workspace allocations cleanly.
    drop(codec);
    assert_eq!(provider.budget_snapshot().unwrap().used_bytes, 0);
    eprintln!(
        "VULKAN_DEVICE_RESIDENT_LZ4_OK device={:?} frames={pool_frames}",
        provider.device_name()
    );
}
