// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cuda/src/ptx_lz4_codec/tests.rs
#![allow(clippy::unwrap_used)]
use super::*;
use crate::driver::Cuda;
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

fn high_entropy_page() -> [u8; PAGE_BYTES] {
    let mut page = [0u8; PAGE_BYTES];
    let mut state = 0x9E37_79B9u32;
    for byte in &mut page {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        *byte = state as u8;
    }
    page
}

#[test]
fn cuda_ptx_roundtrip_and_incompressible_raw_fallback() {
    let mut codec =
        CudaPtxLz4Codec::new(RESERVE_FLOOR_EXTRA_BYTES + 4 * FRAME_BYTES as u64).unwrap();
    let mut pool = VramSlabPool::new(4).unwrap();
    let pages = [compressible_page(2), high_entropy_page()];
    let mut tickets = [None; 2];
    codec
        .compress_and_pack_batch(&pages, &mut pool, &mut tickets)
        .unwrap();
    assert_eq!(tickets[1].unwrap().class, SlabClass::Raw4096);
    assert_eq!(tickets[1].unwrap().encoding, SlotEncoding::Raw);
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
    assert!(codec.hardware_ptx_embedded());
    assert!(validate_embedded_ptx_image(EMBEDDED_LZ4_CRC32_PTX));
    assert!(!validate_embedded_ptx_image(
        b".version 7.0\n.target sm_50\n"
    ));
    let mut corrupt = EMBEDDED_LZ4_CRC32_PTX.to_vec();
    corrupt[0] = b'X';
    assert!(!validate_embedded_ptx_image(&corrupt));
}

#[test]
fn cuda_ptx_bitflip_caught_by_gpu_predecode_crc32_before_lz4() {
    let mut codec =
        CudaPtxLz4Codec::new(RESERVE_FLOOR_EXTRA_BYTES + 2 * FRAME_BYTES as u64).unwrap();
    let mut pool = VramSlabPool::new(2).unwrap();
    let pages = [compressible_page(5), compressible_page(6)];
    let mut tickets = [None; 2];
    codec
        .compress_and_pack_batch(&pages, &mut pool, &mut tickets)
        .unwrap();
    let first = tickets[0].unwrap();
    codec.reference_payload_mut(&first).unwrap()[0] ^= 0x80;
    let mut decoded = [[0u8; PAGE_BYTES]; 2];
    let mut status = [PageDecodeStatus::Ok; 2];
    codec
        .verify_and_unpack_batch(&[first, tickets[1].unwrap()], &mut decoded, &mut status)
        .unwrap();
    assert_eq!(status[0], PageDecodeStatus::PreDecodeCrcMismatch);
    assert_eq!(status[1], PageDecodeStatus::Ok);
}

#[test]
fn cuda_ptx_deadline_exceeded_poisons_without_uaf() {
    let mut codec = CudaPtxLz4Codec::new(RESERVE_FLOOR_EXTRA_BYTES + FRAME_BYTES as u64).unwrap();
    codec.set_force_deadline(true);
    let mut pool = VramSlabPool::new(1).unwrap();
    let mut tickets = [None; 1];
    let error = codec
        .compress_and_pack_batch(&[compressible_page(1)], &mut pool, &mut tickets)
        .unwrap_err();
    assert_eq!(error, CodecError::DeadlineExceeded);
    assert!(codec.poisoned());
    assert!(codec.staging().retained());
    assert_eq!(pool.logical_pages_cached(), 0);
}

#[test]
fn worker_reserve_floor_preaccounts_staging_arena() {
    assert!(CudaPtxLz4Codec::new(RESERVE_FLOOR_EXTRA_BYTES - 1).is_err());
    let codec = CudaPtxLz4Codec::new(RESERVE_FLOOR_EXTRA_BYTES + 2 * FRAME_BYTES as u64).unwrap();
    assert_eq!(codec.staging().usable_frames(), 2);
    for gib in [6u64, 32, 96, 192, 256] {
        let bytes = gib * 1024 * 1024 * 1024;
        let arena = DeviceStagingArena::preaccount(bytes).unwrap();
        assert_eq!(
            u64::from(arena.usable_frames()),
            (bytes - RESERVE_FLOOR_EXTRA_BYTES) / FRAME_BYTES as u64
        );
    }
    assert_eq!(
        DeviceStagingArena::preaccount(u64::MAX)
            .unwrap()
            .usable_frames(),
        u32::MAX
    );
}

#[test]
fn embedded_ptx_image_passes_structural_validator() {
    assert!(validate_embedded_ptx_image(EMBEDDED_LZ4_CRC32_PTX));
    let len = EMBEDDED_LZ4_CRC32_PTX.len();
    for take in [0usize, 8, len / 2, len - 1] {
        assert!(!validate_embedded_ptx_image(
            &EMBEDDED_LZ4_CRC32_PTX[..take]
        ));
    }
    let mut no_nul = EMBEDDED_LZ4_CRC32_PTX[..len - 1].to_vec();
    no_nul.push(b' ');
    assert!(!validate_embedded_ptx_image(&no_nul));
}

#[test]
#[ignore = "explicit native CUDA kernel test; requires a visible GPU and bounded lab headroom"]
fn live_rtx2060_gpu_ptx_crc32_and_lz4_kernel_execution() {
    let cuda = Cuda::load().unwrap_or_else(|error| panic!("live CUDA prerequisite: {error}"));
    let count = cuda
        .device_count()
        .unwrap_or_else(|error| panic!("live CUDA prerequisite: {error}"));
    if std::env::var_os("RAMSHARED_TEST_CUDA_MASK_REQUIRED").is_some() {
        assert_eq!(
            count, 0,
            "CUDA test isolation failed: mask still exposes devices; refusing allocations"
        );
    }
    assert!(count > 0, "live CUDA prerequisite: no visible devices");
    let device = cuda.device(0).unwrap();
    let ctx = cuda.create_context(&device).unwrap();
    let mut codec =
        CudaPtxLz4Codec::new(RESERVE_FLOOR_EXTRA_BYTES + 4 * FRAME_BYTES as u64).unwrap();
    let mut pool = VramSlabPool::new(4).unwrap();
    let pages = [compressible_page(0x42), high_entropy_page()];
    let mut tickets = [None; 2];
    codec
        .compress_and_pack_batch(&pages, &mut pool, &mut tickets)
        .unwrap();
    let lz4_ticket = tickets[0].unwrap();
    let raw_ticket = tickets[1].unwrap();
    assert_eq!(lz4_ticket.encoding, SlotEncoding::Lz4);
    assert_eq!(raw_ticket.encoding, SlotEncoding::Raw);

    // 1. Legitimate LZ4 4 KiB page decoded on the physical GPU via cuLaunchKernel
    let mut gpu_out = [0u8; PAGE_BYTES];
    let status = codec
        .verify_and_unpack_ticket_on_gpu(&ctx, &lz4_ticket, &mut gpu_out)
        .unwrap();
    assert_eq!(status, PageDecodeStatus::Ok);
    assert_eq!(gpu_out, pages[0]);

    // 2. Legitimate Raw 4 KiB page verified by GPU CRC32 kernel
    let mut raw_out = [0u8; PAGE_BYTES];
    let status_raw = codec
        .verify_and_unpack_ticket_on_gpu(&ctx, &raw_ticket, &mut raw_out)
        .unwrap();
    assert_eq!(status_raw, PageDecodeStatus::Ok);
    assert_eq!(raw_out, pages[1]);

    // 3. Bit-flip caught by GPU CRC32 kernel before LZ4 decode
    codec.reference_payload_mut(&lz4_ticket).unwrap()[0] ^= 0x01;
    let status_crc = codec
        .verify_and_unpack_ticket_on_gpu(&ctx, &lz4_ticket, &mut gpu_out)
        .unwrap();
    assert_eq!(status_crc, PageDecodeStatus::PreDecodeCrcMismatch);

    // 4. Corrupt LZ4 stream (with recomputed valid CRC32) rejected by GPU LZ4 kernel
    let payload = codec.reference_payload_mut(&lz4_ticket).unwrap();
    payload.fill(0xFF);
    let forged_crc = crc32(payload, 0);
    let mut corrupt_ticket = lz4_ticket;
    corrupt_ticket.payload_crc32 = forged_crc;
    let status_corrupt = codec
        .verify_and_unpack_ticket_on_gpu(&ctx, &corrupt_ticket, &mut gpu_out)
        .unwrap();
    assert_eq!(status_corrupt, PageDecodeStatus::CorruptLz4Stream);

    // 5. Invalidated ticket returns InvalidTicket
    codec.invalidate(&lz4_ticket);
    let status_invalid = codec
        .verify_and_unpack_ticket_on_gpu(&ctx, &lz4_ticket, &mut gpu_out)
        .unwrap();
    assert_eq!(status_invalid, PageDecodeStatus::InvalidTicket);
    eprintln!("CUDA_KERNEL_DATA_CHECKS_OK device={:?}", device.name());
}

#[test]
fn cuda_live_kernel_refuses_unavailable_device() {
    let executable = std::env::current_exe().unwrap();
    let native_case = "ptx_lz4_codec::tests::live_rtx2060_gpu_ptx_crc32_and_lz4_kernel_execution";
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
    let output = std::process::Command::new(&executable)
        .args([
            native_case,
            "--exact",
            "--include-ignored",
            "--nocapture",
            "--color",
            "never",
        ])
        .env("CUDA_VISIBLE_DEVICES", "-1")
        .env("RAMSHARED_TEST_CUDA_MASK_REQUIRED", "1")
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
        text.contains("live CUDA prerequisite:"),
        "unexpected failure: {text}"
    );
    assert!(!text.contains("CUDA_KERNEL_DATA_CHECKS_OK"));
    assert!(
        text.contains("test result: FAILED. 0 passed; 1 failed"),
        "the native case must actually execute and fail: {text}"
    );
}

#[test]
fn embedded_ptx_image_validates_single_and_batch_hardware_kernels() {
    let text = std::str::from_utf8(EMBEDDED_LZ4_CRC32_PTX).unwrap();
    for entry in [
        "ramshared_crc32_verify_kernel",
        "ramshared_lz4_block_codec_kernel",
        "ramshared_crc32_and_lz4_unpack_kernel",
        "ramshared_batch_verify_and_unpack_kernel",
        "ramshared_batch_lz4_compress_crc32_kernel",
    ] {
        assert!(text.contains(entry), "missing embedded kernel {entry}");
        let redacted = text.replacen(entry, "ramshared_removed_kernel", 1);
        assert!(
            !validate_embedded_ptx_image(redacted.as_bytes()),
            "validator must reject PTX missing {entry}"
        );
    }
}

#[test]
#[ignore = "explicit native CUDA kernel test; requires a visible GPU and bounded lab headroom"]
fn live_rtx2060_gpu_context_ptx_batch_compress_unpack_bitflip_and_poison() {
    let cuda = Cuda::load().unwrap_or_else(|error| panic!("live CUDA prerequisite: {error}"));
    let count = cuda
        .device_count()
        .unwrap_or_else(|error| panic!("live CUDA prerequisite: {error}"));
    assert!(count > 0, "live CUDA prerequisite: no visible devices");
    let device = cuda.device(0).unwrap();
    let ctx = cuda.create_context(&device).unwrap();

    assert!(matches!(
        CudaPtxLz4Codec::new_context_codec(
            &ctx,
            RESERVE_FLOOR_EXTRA_BYTES + 4 * FRAME_BYTES as u64,
            0
        ),
        Err(ramshared_vram::CodecError::BackendFailure(_))
    ));
    assert!(matches!(
        CudaPtxLz4Codec::new_context_codec(
            &ctx,
            RESERVE_FLOOR_EXTRA_BYTES + 2 * FRAME_BYTES as u64,
            4
        ),
        Err(ramshared_vram::CodecError::BackendFailure(_))
    ));

    let available = RESERVE_FLOOR_EXTRA_BYTES + 8 * FRAME_BYTES as u64;
    let mut codec = CudaPtxLz4Codec::new_context_codec(&ctx, available, 8).unwrap();
    assert_eq!(
        codec.device_allocation_bytes(),
        Some((8 * FRAME_BYTES as u64, STAGING_TOTAL_BYTES))
    );
    assert_eq!(codec.backend_name(), "cuda-ptx-gpu");
    let mut pool = VramSlabPool::new(8).unwrap();

    let mut pages = Vec::with_capacity(16);
    for i in 0..16_usize {
        if i < 4 {
            pages.push([0u8; PAGE_BYTES]);
        } else if i < 12 {
            pages.push(compressible_page(0x30 + i as u8));
        } else {
            let mut state = 0x9e37_79b9_7f4a_7c15_u64 ^ ((i as u64 + 1) << 32);
            let mut entropy = [0u8; PAGE_BYTES];
            for b in &mut entropy {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                *b = (state >> 24) as u8;
            }
            pages.push(entropy);
        }
    }

    let mut tickets = [None; 16];
    codec
        .compress_and_pack_batch(&pages, &mut pool, &mut tickets)
        .unwrap();
    assert_eq!(codec.kernels_launched(), 1);
    assert!(codec.events_polled() >= 1);

    let unpacked_tickets: Vec<VramSlabTicket> = tickets.iter().map(|t| t.unwrap()).collect();
    for (idx, t) in unpacked_tickets.iter().enumerate() {
        if idx < 12 {
            assert_eq!(t.encoding, SlotEncoding::Lz4, "page {idx} must compress");
            assert!(usize::from(t.payload_len) <= ramshared_vram::MAX_COMPRESSED_LEN);
        } else {
            assert_eq!(
                t.encoding,
                SlotEncoding::Raw,
                "entropy page {idx} must stay raw"
            );
            assert_eq!(usize::from(t.payload_len), PAGE_BYTES);
        }
    }

    let mut unpacked = vec![[0u8; PAGE_BYTES]; 16];
    let mut statuses = vec![PageDecodeStatus::InvalidTicket; 16];
    codec
        .verify_and_unpack_batch(&unpacked_tickets, &mut unpacked, &mut statuses)
        .unwrap();
    assert_eq!(codec.kernels_launched(), 2);
    for i in 0..16 {
        assert_eq!(statuses[i], PageDecodeStatus::Ok, "page {i} decode status");
        assert_eq!(unpacked[i], pages[i], "page {i} byte mismatch");
    }

    // Inject real VRAM tamper into slot 0 and invalidate slot 5; batch must report
    // exact per-page statuses while cleanly decoding healthy lanes.
    codec.inject_vram_bitflip(&unpacked_tickets[0], 0, 0x01);
    codec.invalidate(&unpacked_tickets[5]);
    unpacked.fill([0u8; PAGE_BYTES]);
    statuses.fill(PageDecodeStatus::Ok);
    codec
        .verify_and_unpack_batch(&unpacked_tickets, &mut unpacked, &mut statuses)
        .unwrap();
    assert_eq!(statuses[0], PageDecodeStatus::PreDecodeCrcMismatch);
    assert_eq!(statuses[5], PageDecodeStatus::InvalidTicket);
    for i in 1..16 {
        if i == 5 {
            continue;
        }
        assert_eq!(statuses[i], PageDecodeStatus::Ok);
        assert_eq!(unpacked[i], pages[i]);
    }

    // Timeout deadline must poison the codec, retain VRAM (DT-3), and never panic on Drop.
    codec.set_force_deadline(true);
    let err = codec.verify_and_unpack_batch(
        &unpacked_tickets[..1],
        &mut unpacked[..1],
        &mut statuses[..1],
    );
    assert_eq!(err, Err(ramshared_vram::CodecError::DeadlineExceeded));
    assert!(codec.poisoned());
    assert!(codec.staging().retained());
    drop(codec);
    eprintln!("CUDA_BATCH_GPU_CODEC_OK device={:?}", device.name());
}

#[test]
fn mock_context_ptx_codec_and_facade_cover_all_software_paths() {
    use core::cell::Cell;
    use core::ffi::{CStr, c_char, c_uint, c_void};
    use std::time::Duration;

    use crate::driver::tests::{mock_cuda, with_mock_device_range};
    use crate::ffi::{CUDA_SUCCESS, CuDevicePtr, CuResult};
    use ramshared_vram::device_codec::{lz4_compress_block, lz4_decompress_block};

    const FN_BATCH_COMPRESS: *mut c_void = std::ptr::without_provenance_mut(1);
    const FN_BATCH_UNPACK: *mut c_void = std::ptr::without_provenance_mut(2);
    const FN_SINGLE_UNPACK: *mut c_void = std::ptr::without_provenance_mut(3);

    thread_local! {
        static QUERY_MODE: Cell<u8> = const { Cell::new(0) };
        static FAIL_STEP: Cell<u8> = const { Cell::new(0) };
    }

    unsafe extern "C" fn mock_mod_load(m: *mut syms::CuModule, _: *const c_void) -> CuResult {
        if FAIL_STEP.with(|f| f.get()) == 1 {
            return 301;
        }
        unsafe { *m = std::ptr::without_provenance_mut(0xAA) };
        CUDA_SUCCESS
    }
    unsafe extern "C" fn mock_mod_unload(_: syms::CuModule) -> CuResult {
        CUDA_SUCCESS
    }
    unsafe extern "C" fn mock_mod_fn(
        f: *mut syms::CuFunction,
        _: syms::CuModule,
        name: *const c_char,
    ) -> CuResult {
        if FAIL_STEP.with(|x| x.get()) == 2 {
            return 302;
        }
        let s = unsafe { CStr::from_ptr(name) }.to_str().unwrap_or("");
        unsafe {
            *f = match s {
                "ramshared_batch_lz4_compress_crc32_kernel" => FN_BATCH_COMPRESS,
                "ramshared_batch_verify_and_unpack_kernel" => FN_BATCH_UNPACK,
                _ => FN_SINGLE_UNPACK,
            };
        }
        CUDA_SUCCESS
    }
    unsafe extern "C" fn mock_ev_create(ev: *mut syms::CuEvent, _: c_uint) -> CuResult {
        if FAIL_STEP.with(|x| x.get()) == 3 {
            return 303;
        }
        unsafe { *ev = std::ptr::without_provenance_mut(0xBB) };
        CUDA_SUCCESS
    }
    unsafe extern "C" fn mock_ev_destroy(_: syms::CuEvent) -> CuResult {
        CUDA_SUCCESS
    }
    unsafe extern "C" fn mock_ev_record(_: syms::CuEvent, _: syms::CuStream) -> CuResult {
        if FAIL_STEP.with(|x| x.get()) == 5 {
            return 305;
        }
        CUDA_SUCCESS
    }
    unsafe extern "C" fn mock_ev_query(_: syms::CuEvent) -> CuResult {
        match QUERY_MODE.with(|m| m.get()) {
            0 => CUDA_SUCCESS,
            1 => 600,
            _ => 999,
        }
    }
    unsafe extern "C" fn mock_dtod(dst: CuDevicePtr, src: CuDevicePtr, len: usize) -> CuResult {
        if FAIL_STEP.with(|x| x.get()) == 6 {
            return 306;
        }
        let Some(bytes) = with_mock_device_range(src, len, |s| s.to_vec()) else {
            return 1;
        };
        if with_mock_device_range(dst, len, |d| d.copy_from_slice(&bytes)).is_none() {
            return 1;
        }
        CUDA_SUCCESS
    }
    unsafe extern "C" fn mock_launch(
        func: syms::CuFunction,
        grid_x: c_uint,
        _: c_uint,
        _: c_uint,
        _: c_uint,
        _: c_uint,
        _: c_uint,
        _: c_uint,
        _: syms::CuStream,
        params: *mut *mut c_void,
        _: *mut *mut c_void,
    ) -> CuResult {
        if FAIL_STEP.with(|x| x.get()) == 4 {
            return 304;
        }
        unsafe {
            if func == FN_BATCH_COMPRESS {
                let src = *((*params.add(0)) as *const u64);
                let dst = *((*params.add(1)) as *const u64);
                let lens_p = *((*params.add(3)) as *const u64);
                let crcs_p = *((*params.add(4)) as *const u64);
                let encs_p = *((*params.add(5)) as *const u64);
                for i in 0..grid_x as usize {
                    let page =
                        with_mock_device_range(src + (i * PAGE_BYTES) as u64, PAGE_BYTES, |b| {
                            b.to_vec()
                        })
                        .unwrap();
                    let (enc, payload) = match lz4_compress_block(&page) {
                        Some(comp) if comp.len() <= ramshared_vram::MAX_COMPRESSED_LEN => {
                            (syms::ENCODING_LZ4, comp)
                        }
                        _ => (syms::ENCODING_RAW, page),
                    };
                    let c = crc32(&payload, 0);
                    with_mock_device_range(dst + (i * PAGE_BYTES) as u64, payload.len(), |b| {
                        b.copy_from_slice(&payload)
                    });
                    with_mock_device_range(lens_p + (i * 4) as u64, 4, |b| {
                        b.copy_from_slice(&(payload.len() as u32).to_le_bytes())
                    });
                    with_mock_device_range(crcs_p + (i * 4) as u64, 4, |b| {
                        b.copy_from_slice(&c.to_le_bytes())
                    });
                    with_mock_device_range(encs_p + (i * 4) as u64, 4, |b| {
                        b.copy_from_slice(&enc.to_le_bytes())
                    });
                }
            } else if func == FN_BATCH_UNPACK {
                let slab = *((*params.add(0)) as *const u64);
                let offs_p = *((*params.add(2)) as *const u64);
                let lens_p = *((*params.add(3)) as *const u64);
                let crcs_p = *((*params.add(4)) as *const u64);
                let encs_p = *((*params.add(5)) as *const u64);
                let dst = *((*params.add(6)) as *const u64);
                let status_p = *((*params.add(8)) as *const u64);
                for i in 0..grid_x as usize {
                    let mut u64b = [0u8; 8];
                    let mut u32b = [0u8; 4];
                    with_mock_device_range(lens_p + (i * 4) as u64, 4, |b| u32b.copy_from_slice(b));
                    let len = u32::from_le_bytes(u32b) as usize;
                    if len == 0 {
                        continue;
                    }
                    with_mock_device_range(offs_p + (i * 8) as u64, 8, |b| u64b.copy_from_slice(b));
                    let off = u64::from_le_bytes(u64b);
                    with_mock_device_range(crcs_p + (i * 4) as u64, 4, |b| u32b.copy_from_slice(b));
                    let ecrc = u32::from_le_bytes(u32b);
                    with_mock_device_range(encs_p + (i * 4) as u64, 4, |b| u32b.copy_from_slice(b));
                    let enc = u32::from_le_bytes(u32b);
                    let src_bytes =
                        with_mock_device_range(slab + off, len, |b| b.to_vec()).unwrap();
                    let st = if crc32(&src_bytes, 0) != ecrc {
                        1u8
                    } else if enc == syms::ENCODING_RAW {
                        if len == PAGE_BYTES {
                            with_mock_device_range(
                                dst + (i * PAGE_BYTES) as u64,
                                PAGE_BYTES,
                                |b| b.copy_from_slice(&src_bytes),
                            );
                            0u8
                        } else {
                            2u8
                        }
                    } else {
                        let mut out = [0u8; PAGE_BYTES];
                        if lz4_decompress_block(&src_bytes, &mut out).is_ok() {
                            with_mock_device_range(
                                dst + (i * PAGE_BYTES) as u64,
                                PAGE_BYTES,
                                |b| b.copy_from_slice(&out),
                            );
                            0u8
                        } else {
                            2u8
                        }
                    };
                    with_mock_device_range(status_p + i as u64, 1, |b| b[0] = st);
                }
            } else {
                let src = *((*params.add(0)) as *const u64);
                let len = *((*params.add(1)) as *const u32) as usize;
                let ecrc = *((*params.add(2)) as *const u32);
                let enc = *((*params.add(3)) as *const u32);
                let dst = *((*params.add(4)) as *const u64);
                let status_p = *((*params.add(6)) as *const u64);
                let src_bytes = with_mock_device_range(src, len, |b| b.to_vec()).unwrap();
                let st = if crc32(&src_bytes, 0) != ecrc {
                    1u8
                } else if enc == syms::ENCODING_RAW && len == PAGE_BYTES {
                    with_mock_device_range(dst, PAGE_BYTES, |b| b.copy_from_slice(&src_bytes));
                    0u8
                } else {
                    let mut out = [0u8; PAGE_BYTES];
                    if lz4_decompress_block(&src_bytes, &mut out).is_ok() {
                        with_mock_device_range(dst, PAGE_BYTES, |b| b.copy_from_slice(&out));
                        0u8
                    } else {
                        2u8
                    }
                };
                with_mock_device_range(status_p, 1, |b| b[0] = st);
            }
        }
        CUDA_SUCCESS
    }

    let mock_syms = syms::KernelSyms {
        module_load_data: mock_mod_load,
        module_unload: mock_mod_unload,
        module_get_function: mock_mod_fn,
        launch_kernel: mock_launch,
        event_create: mock_ev_create,
        event_destroy: mock_ev_destroy,
        event_record: mock_ev_record,
        event_query: mock_ev_query,
        memcpy_dtod: mock_dtod,
    };

    let cuda = mock_cuda(None);
    let dev = cuda.device(0).unwrap();
    let ctx = cuda.create_context(&dev).unwrap();
    let avail = RESERVE_FLOOR_EXTRA_BYTES + 4 * FRAME_BYTES as u64;

    // 1. Real symbol loader fails cleanly when no libcuda handle is attached.
    assert!(CudaPtxLz4Codec::new_context_codec(&ctx, avail, 1).is_err());

    // 2. Facade methods & single-kernel mock path.
    let mut facade = CudaPtxLz4Codec::new(1024 * 1024 * 1024).unwrap();
    assert_eq!(facade.backend_name(), "cuda-ptx");
    let mut fpool = VramSlabPool::new(4).unwrap();
    let mut ftickets = [None; 2];
    let cp = compressible_page(0x41);
    let hp = high_entropy_page();
    facade
        .compress_and_pack_batch(&[cp, hp], &mut fpool, &mut ftickets)
        .unwrap();
    let t0 = ftickets[0].unwrap();
    let t1 = ftickets[1].unwrap();
    let mut out_single = [0u8; PAGE_BYTES];
    assert_eq!(
        syms::verify_and_unpack_single_with_syms(
            &ctx,
            &mock_syms,
            facade.reference_payload_mut(&t0).unwrap(),
            t0.payload_crc32,
            t0.encoding,
            &mut out_single
        )
        .unwrap(),
        PageDecodeStatus::Ok
    );
    assert_eq!(out_single, cp);
    assert_eq!(
        syms::verify_and_unpack_single_with_syms(
            &ctx,
            &mock_syms,
            facade.reference_payload_mut(&t1).unwrap(),
            t1.payload_crc32 ^ 1,
            t1.encoding,
            &mut out_single
        )
        .unwrap(),
        PageDecodeStatus::PreDecodeCrcMismatch
    );
    let bad_lz4 = [0xFFu8; 12];
    assert_eq!(
        syms::verify_and_unpack_single_with_syms(
            &ctx,
            &mock_syms,
            &bad_lz4,
            crc32(&bad_lz4, 0),
            SlotEncoding::Lz4,
            &mut out_single
        )
        .unwrap(),
        PageDecodeStatus::CorruptLz4Stream
    );
    facade.invalidate(&t0);
    assert_eq!(
        facade
            .verify_and_unpack_ticket_on_gpu(&ctx, &t0, &mut out_single)
            .unwrap(),
        PageDecodeStatus::InvalidTicket
    );
    assert!(
        facade
            .verify_and_unpack_ticket_on_gpu(&ctx, &t1, &mut out_single)
            .is_err()
    );
    facade.poison();
    assert!(
        facade
            .verify_and_unpack_ticket_on_gpu(&ctx, &t1, &mut out_single)
            .is_err()
    );

    // 3. Init failure paths for CudaContextPtxCodec::new_with_syms.
    assert!(CudaContextPtxCodec::new_with_syms(&ctx, avail, 0, mock_syms).is_err());
    assert!(CudaContextPtxCodec::new_with_syms(&ctx, avail, 999_999, mock_syms).is_err());
    assert!(
        CudaContextPtxCodec::new_with_syms(&ctx, RESERVE_FLOOR_EXTRA_BYTES + 4096, 1, mock_syms)
            .is_err()
    );
    for step in [1u8, 2, 3] {
        FAIL_STEP.with(|f| f.set(step));
        assert!(CudaContextPtxCodec::new_with_syms(&ctx, avail, 2, mock_syms).is_err());
    }
    FAIL_STEP.with(|f| f.set(0));
    QUERY_MODE.with(|m| m.set(0));

    // 4. Clean non-poisoned batch lifecycle + error arms + Drop.
    let mut codec = CudaContextPtxCodec::new_with_syms(&ctx, avail, 2, mock_syms).unwrap();
    assert_eq!(
        codec.device_allocation_bytes(),
        Some((2 * FRAME_BYTES as u64, STAGING_TOTAL_BYTES))
    );
    let mut pool = VramSlabPool::new(2).unwrap();
    let mut t_empty: [Option<VramSlabTicket>; 0] = [];
    codec
        .compress_and_pack_batch(&[], &mut pool, &mut t_empty)
        .unwrap();
    assert!(
        codec
            .compress_and_pack_batch(&[cp, hp], &mut pool, &mut [None; 1])
            .is_err()
    );
    let mut tickets = [None; 2];
    codec
        .compress_and_pack_batch(&[cp, hp], &mut pool, &mut tickets)
        .unwrap();
    let k0 = tickets[0].unwrap();
    let k1 = tickets[1].unwrap();
    let mut out = [[0u8; PAGE_BYTES]; 2];
    let mut st = [PageDecodeStatus::InvalidTicket; 2];
    codec
        .verify_and_unpack_batch(&[], &mut [], &mut [])
        .unwrap();
    assert!(
        codec
            .verify_and_unpack_batch(&[k0], &mut [], &mut st)
            .is_err()
    );
    codec
        .verify_and_unpack_batch(&[k0, k1], &mut out, &mut st)
        .unwrap();
    assert_eq!(st, [PageDecodeStatus::Ok, PageDecodeStatus::Ok]);
    assert_eq!(out[0], cp);
    assert_eq!(out[1], hp);

    // Tamper & invalidate recovery.
    codec.inject_vram_bitflip(&k0, 0, 0x01);
    codec.invalidate(&k1);
    codec
        .verify_and_unpack_batch(&[k0, k1], &mut out, &mut st)
        .unwrap();
    assert_eq!(
        st,
        [
            PageDecodeStatus::PreDecodeCrcMismatch,
            PageDecodeStatus::InvalidTicket
        ]
    );
    drop(codec);

    // 5. Launch/dtod/deadline/event errors & poisoned Drop.
    let mut c2 = CudaContextPtxCodec::new_with_syms(&ctx, avail, 2, mock_syms).unwrap();
    let mut p2 = VramSlabPool::new(2).unwrap();
    FAIL_STEP.with(|f| f.set(6));
    assert!(
        c2.compress_and_pack_batch(&[cp], &mut p2, &mut [None; 1])
            .is_err()
    );
    FAIL_STEP.with(|f| f.set(0));
    QUERY_MODE.with(|m| m.set(2));
    assert!(
        c2.compress_and_pack_batch(&[cp], &mut p2, &mut [None; 1])
            .is_err()
    );
    assert!(c2.poisoned());
    drop(c2);

    let mut c3 = CudaContextPtxCodec::new_with_syms(&ctx, avail, 2, mock_syms).unwrap();
    c3.set_deadline(Duration::ZERO);
    QUERY_MODE.with(|m| m.set(1));
    let mut p3 = VramSlabPool::new(2).unwrap();
    assert_eq!(
        c3.compress_and_pack_batch(&[cp], &mut p3, &mut [None; 1]),
        Err(ramshared_vram::CodecError::DeadlineExceeded)
    );
    assert!(c3.poisoned());
    QUERY_MODE.with(|m| m.set(0));
}

#[test]
fn staging_budget_accounts_for_all_three_device_batches_and_metadata() {
    let actual = (3 * ramshared_vram::device_codec::MAX_BATCH_PAGES * PAGE_BYTES) as u64
        + STAGING_META_BYTES;
    assert_eq!(STAGING_TOTAL_BYTES, actual);
    assert!(DeviceStagingArena::preaccount(actual).is_err());
    assert!(DeviceStagingArena::preaccount(actual + FRAME_BYTES as u64).is_ok());
}
