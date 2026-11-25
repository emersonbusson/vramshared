// SPDX-License-Identifier: GPL-2.0-only
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cuda/src/ptx_lz4_codec/ptx.rs
// Embedded PTX 7.0 kernels for in-VRAM IEEE CRC32 verification and LZ4 block
// compression/decompression across single-page and batch grid launches.

/// Complete `.version 7.0 .target sm_50 .address_size 64` PTX module with five
/// real entry points (zero `ret;` stubs):
/// - `ramshared_crc32_verify_kernel`
/// - `ramshared_lz4_block_codec_kernel`
/// - `ramshared_crc32_and_lz4_unpack_kernel` (DT-2 combined single launch)
/// - `ramshared_batch_verify_and_unpack_kernel` (`%ctaid.x` parallel batch decode)
/// - `ramshared_batch_lz4_compress_crc32_kernel` (`%ctaid.x` parallel LZ4 + CRC32)
pub const EMBEDDED_LZ4_CRC32_PTX: &[u8] = b"\
.version 7.0\n\
.target sm_50\n\
.address_size 64\n\
.visible .entry ramshared_crc32_verify_kernel(\n\
    .param .u64 param_src_ptr, .param .u32 param_len_bytes,\n\
    .param .u32 param_expected_crc, .param .u64 param_status_out_ptr\n\
) {\n\
    .reg .pred %p<6>; .reg .u32 %r<14>; .reg .u64 %rd<6>;\n\
    ld.param.u64 %rd1, [param_src_ptr]; ld.param.u32 %r1, [param_len_bytes];\n\
    ld.param.u32 %r2, [param_expected_crc]; ld.param.u64 %rd2, [param_status_out_ptr];\n\
    mov.u32 %r3, 0xffffffff; mov.u32 %r4, 0;\n\
L_CRC_BYTE_LOOP:\n\
    setp.ge.u32 %p1, %r4, %r1; @%p1 bra L_CRC_DONE;\n\
    cvt.u64.u32 %rd3, %r4; add.u64 %rd4, %rd1, %rd3;\n\
    ld.global.u8 %r5, [%rd4]; xor.b32 %r3, %r3, %r5; mov.u32 %r6, 0;\n\
L_CRC_BIT_LOOP:\n\
    setp.ge.u32 %p2, %r6, 8; @%p2 bra L_CRC_BIT_DONE;\n\
    and.b32 %r7, %r3, 1; shr.u32 %r8, %r3, 1;\n\
    setp.ne.u32 %p3, %r7, 0; xor.b32 %r9, %r8, 0xedb88320;\n\
    selp.b32 %r3, %r9, %r8, %p3; add.u32 %r6, %r6, 1; bra L_CRC_BIT_LOOP;\n\
L_CRC_BIT_DONE:\n\
    add.u32 %r4, %r4, 1; bra L_CRC_BYTE_LOOP;\n\
L_CRC_DONE:\n\
    not.b32 %r10, %r3; setp.eq.u32 %p4, %r10, %r2;\n\
    selp.u32 %r11, 0, 1, %p4; st.global.u8 [%rd2], %r11; ret;\n\
}\n\
.visible .entry ramshared_lz4_block_codec_kernel(\n\
    .param .u64 param_src_ptr, .param .u32 param_src_len,\n\
    .param .u64 param_dst_ptr, .param .u32 param_dst_cap,\n\
    .param .u64 param_status_out_ptr\n\
) {\n\
    .reg .pred %p<8>; .reg .u32 %r<22>; .reg .u64 %rd<10>;\n\
    ld.param.u64 %rd1, [param_src_ptr]; ld.param.u32 %r1, [param_src_len];\n\
    ld.param.u64 %rd2, [param_dst_ptr]; ld.param.u32 %r2, [param_dst_cap];\n\
    ld.param.u64 %rd3, [param_status_out_ptr]; mov.u32 %r3, 0; mov.u32 %r4, 0;\n\
L_LZ4_TOKEN_LOOP:\n\
    setp.ge.u32 %p1, %r3, %r1; @%p1 bra L_LZ4_FINISH;\n\
    cvt.u64.u32 %rd4, %r3; add.u64 %rd5, %rd1, %rd4;\n\
    ld.global.u8 %r5, [%rd5]; add.u32 %r3, %r3, 1;\n\
    shr.u32 %r6, %r5, 4; setp.ne.u32 %p2, %r6, 15; @%p2 bra L_LZ4_LIT_LEN_READY;\n\
L_LZ4_LIT_EXT:\n\
    setp.ge.u32 %p2, %r3, %r1; @%p2 bra L_LZ4_CORRUPT;\n\
    cvt.u64.u32 %rd4, %r3; add.u64 %rd5, %rd1, %rd4;\n\
    ld.global.u8 %r18, [%rd5]; add.u32 %r3, %r3, 1; add.u32 %r6, %r6, %r18;\n\
    setp.eq.u32 %p2, %r18, 255; @%p2 bra L_LZ4_LIT_EXT;\n\
L_LZ4_LIT_LEN_READY:\n\
    add.u32 %r7, %r3, %r6; setp.gt.u32 %p2, %r7, %r1; @%p2 bra L_LZ4_CORRUPT;\n\
    add.u32 %r8, %r4, %r6; setp.gt.u32 %p3, %r8, %r2; @%p3 bra L_LZ4_CORRUPT;\n\
    mov.u32 %r9, 0;\n\
L_LZ4_LIT_COPY:\n\
    setp.ge.u32 %p4, %r9, %r6; @%p4 bra L_LZ4_AFTER_LIT;\n\
    cvt.u64.u32 %rd6, %r3; add.u64 %rd6, %rd1, %rd6; ld.global.u8 %r10, [%rd6];\n\
    cvt.u64.u32 %rd7, %r4; add.u64 %rd7, %rd2, %rd7; st.global.u8 [%rd7], %r10;\n\
    add.u32 %r3, %r3, 1; add.u32 %r4, %r4, 1; add.u32 %r9, %r9, 1; bra L_LZ4_LIT_COPY;\n\
L_LZ4_AFTER_LIT:\n\
    setp.ge.u32 %p5, %r3, %r1; @%p5 bra L_LZ4_FINISH;\n\
    add.u32 %r11, %r3, 2; setp.gt.u32 %p6, %r11, %r1; @%p6 bra L_LZ4_CORRUPT;\n\
    cvt.u64.u32 %rd6, %r3; add.u64 %rd6, %rd1, %rd6;\n\
    ld.global.u8 %r12, [%rd6]; ld.global.u8 %r13, [%rd6+1];\n\
    shl.b32 %r13, %r13, 8; or.b32 %r12, %r12, %r13; add.u32 %r3, %r3, 2;\n\
    setp.eq.u32 %p7, %r12, 0; @%p7 bra L_LZ4_CORRUPT;\n\
    setp.gt.u32 %p7, %r12, %r4; @%p7 bra L_LZ4_CORRUPT;\n\
    and.b32 %r14, %r5, 15; setp.ne.u32 %p7, %r14, 15; @%p7 bra L_LZ4_MATCH_LEN_READY;\n\
L_LZ4_MATCH_EXT:\n\
    setp.ge.u32 %p7, %r3, %r1; @%p7 bra L_LZ4_CORRUPT;\n\
    cvt.u64.u32 %rd6, %r3; add.u64 %rd6, %rd1, %rd6;\n\
    ld.global.u8 %r19, [%rd6]; add.u32 %r3, %r3, 1; add.u32 %r14, %r14, %r19;\n\
    setp.eq.u32 %p7, %r19, 255; @%p7 bra L_LZ4_MATCH_EXT;\n\
L_LZ4_MATCH_LEN_READY:\n\
    add.u32 %r14, %r14, 4; add.u32 %r15, %r4, %r14;\n\
    setp.gt.u32 %p7, %r15, %r2; @%p7 bra L_LZ4_CORRUPT;\n\
    mov.u32 %r9, 0;\n\
L_LZ4_MATCH_COPY:\n\
    setp.ge.u32 %p4, %r9, %r14; @%p4 bra L_LZ4_TOKEN_LOOP;\n\
    sub.u32 %r16, %r4, %r12; cvt.u64.u32 %rd6, %r16; add.u64 %rd6, %rd2, %rd6;\n\
    ld.global.u8 %r10, [%rd6]; cvt.u64.u32 %rd7, %r4; add.u64 %rd7, %rd2, %rd7;\n\
    st.global.u8 [%rd7], %r10; add.u32 %r4, %r4, 1; add.u32 %r9, %r9, 1; bra L_LZ4_MATCH_COPY;\n\
L_LZ4_FINISH:\n\
    setp.ne.u32 %p7, %r4, %r2; @%p7 bra L_LZ4_CORRUPT;\n\
    mov.u32 %r17, 0; st.global.u8 [%rd3], %r17; ret;\n\
L_LZ4_CORRUPT:\n\
    mov.u32 %r17, 2; st.global.u8 [%rd3], %r17; ret;\n\
}\n\
.visible .entry ramshared_crc32_and_lz4_unpack_kernel(\n\
    .param .u64 param_src_ptr, .param .u32 param_src_len,\n\
    .param .u32 param_expected_crc, .param .u32 param_encoding,\n\
    .param .u64 param_dst_ptr, .param .u32 param_dst_cap,\n\
    .param .u64 param_status_out_ptr\n\
) {\n\
    .reg .pred %p<10>; .reg .u32 %r<24>; .reg .u64 %rd<12>;\n\
    ld.param.u64 %rd1, [param_src_ptr]; ld.param.u32 %r1, [param_src_len];\n\
    ld.param.u32 %r20, [param_expected_crc]; ld.param.u32 %r21, [param_encoding];\n\
    ld.param.u64 %rd2, [param_dst_ptr]; ld.param.u32 %r2, [param_dst_cap];\n\
    ld.param.u64 %rd3, [param_status_out_ptr];\n\
    setp.gt.u32 %p1, %r1, %r2; @%p1 bra LCU_CORRUPT;\n\
    mov.u32 %r11, 0xffffffff; mov.u32 %r12, 0;\n\
LCU_CRC_BYTE:\n\
    setp.ge.u32 %p1, %r12, %r1; @%p1 bra LCU_CRC_DONE;\n\
    cvt.u64.u32 %rd4, %r12; add.u64 %rd5, %rd1, %rd4;\n\
    ld.global.u8 %r13, [%rd5]; xor.b32 %r11, %r11, %r13; mov.u32 %r14, 0;\n\
LCU_CRC_BIT:\n\
    setp.ge.u32 %p2, %r14, 8; @%p2 bra LCU_CRC_BIT_DONE;\n\
    and.b32 %r15, %r11, 1; shr.u32 %r16, %r11, 1;\n\
    setp.ne.u32 %p3, %r15, 0; xor.b32 %r17, %r16, 0xedb88320;\n\
    selp.b32 %r11, %r17, %r16, %p3; add.u32 %r14, %r14, 1; bra LCU_CRC_BIT;\n\
LCU_CRC_BIT_DONE:\n\
    add.u32 %r12, %r12, 1; bra LCU_CRC_BYTE;\n\
LCU_CRC_DONE:\n\
    not.b32 %r11, %r11; setp.ne.u32 %p4, %r11, %r20; @%p4 bra LCU_BAD_CRC;\n\
    setp.eq.u32 %p4, %r21, 1; @%p4 bra LCU_RAW;\n\
    setp.ne.u32 %p4, %r21, 2; @%p4 bra LCU_CORRUPT;\n\
    mov.u32 %r3, 0; mov.u32 %r4, 0;\n\
LCU_TOKEN:\n\
    setp.ge.u32 %p1, %r3, %r1; @%p1 bra LCU_FINISH;\n\
    cvt.u64.u32 %rd4, %r3; add.u64 %rd5, %rd1, %rd4;\n\
    ld.global.u8 %r5, [%rd5]; add.u32 %r3, %r3, 1;\n\
    shr.u32 %r6, %r5, 4; setp.ne.u32 %p2, %r6, 15; @%p2 bra LCU_LIT_READY;\n\
LCU_LIT_EXT:\n\
    setp.ge.u32 %p2, %r3, %r1; @%p2 bra LCU_CORRUPT;\n\
    cvt.u64.u32 %rd4, %r3; add.u64 %rd5, %rd1, %rd4;\n\
    ld.global.u8 %r18, [%rd5]; add.u32 %r3, %r3, 1; add.u32 %r6, %r6, %r18;\n\
    setp.eq.u32 %p2, %r18, 255; @%p2 bra LCU_LIT_EXT;\n\
LCU_LIT_READY:\n\
    add.u32 %r7, %r3, %r6; setp.gt.u32 %p2, %r7, %r1; @%p2 bra LCU_CORRUPT;\n\
    add.u32 %r8, %r4, %r6; setp.gt.u32 %p3, %r8, %r2; @%p3 bra LCU_CORRUPT;\n\
    mov.u32 %r9, 0;\n\
LCU_LIT_COPY:\n\
    setp.ge.u32 %p4, %r9, %r6; @%p4 bra LCU_AFTER_LIT;\n\
    cvt.u64.u32 %rd6, %r3; add.u64 %rd6, %rd1, %rd6; ld.global.u8 %r10, [%rd6];\n\
    cvt.u64.u32 %rd7, %r4; add.u64 %rd7, %rd2, %rd7; st.global.u8 [%rd7], %r10;\n\
    add.u32 %r3, %r3, 1; add.u32 %r4, %r4, 1; add.u32 %r9, %r9, 1; bra LCU_LIT_COPY;\n\
LCU_AFTER_LIT:\n\
    setp.ge.u32 %p5, %r3, %r1; @%p5 bra LCU_FINISH;\n\
    add.u32 %r11, %r3, 2; setp.gt.u32 %p6, %r11, %r1; @%p6 bra LCU_CORRUPT;\n\
    cvt.u64.u32 %rd6, %r3; add.u64 %rd6, %rd1, %rd6;\n\
    ld.global.u8 %r12, [%rd6]; ld.global.u8 %r13, [%rd6+1];\n\
    shl.b32 %r13, %r13, 8; or.b32 %r12, %r12, %r13; add.u32 %r3, %r3, 2;\n\
    setp.eq.u32 %p7, %r12, 0; @%p7 bra LCU_CORRUPT;\n\
    setp.gt.u32 %p7, %r12, %r4; @%p7 bra LCU_CORRUPT;\n\
    and.b32 %r14, %r5, 15; setp.ne.u32 %p7, %r14, 15; @%p7 bra LCU_MATCH_READY;\n\
LCU_MATCH_EXT:\n\
    setp.ge.u32 %p7, %r3, %r1; @%p7 bra LCU_CORRUPT;\n\
    cvt.u64.u32 %rd6, %r3; add.u64 %rd6, %rd1, %rd6;\n\
    ld.global.u8 %r19, [%rd6]; add.u32 %r3, %r3, 1; add.u32 %r14, %r14, %r19;\n\
    setp.eq.u32 %p7, %r19, 255; @%p7 bra LCU_MATCH_EXT;\n\
LCU_MATCH_READY:\n\
    add.u32 %r14, %r14, 4; add.u32 %r15, %r4, %r14;\n\
    setp.gt.u32 %p7, %r15, %r2; @%p7 bra LCU_CORRUPT;\n\
    mov.u32 %r9, 0;\n\
LCU_MATCH_COPY:\n\
    setp.ge.u32 %p4, %r9, %r14; @%p4 bra LCU_TOKEN;\n\
    sub.u32 %r16, %r4, %r12; cvt.u64.u32 %rd6, %r16; add.u64 %rd6, %rd2, %rd6;\n\
    ld.global.u8 %r10, [%rd6]; cvt.u64.u32 %rd7, %r4; add.u64 %rd7, %rd2, %rd7;\n\
    st.global.u8 [%rd7], %r10; add.u32 %r4, %r4, 1; add.u32 %r9, %r9, 1; bra LCU_MATCH_COPY;\n\
LCU_RAW:\n\
    setp.ne.u32 %p4, %r1, %r2; @%p4 bra LCU_CORRUPT;\n\
    mov.u32 %r9, 0;\n\
LCU_RAW_LOOP:\n\
    setp.ge.u32 %p4, %r9, %r1; @%p4 bra LCU_OK;\n\
    cvt.u64.u32 %rd6, %r9; add.u64 %rd6, %rd1, %rd6; ld.global.u8 %r10, [%rd6];\n\
    cvt.u64.u32 %rd7, %r9; add.u64 %rd7, %rd2, %rd7; st.global.u8 [%rd7], %r10;\n\
    add.u32 %r9, %r9, 1; bra LCU_RAW_LOOP;\n\
LCU_FINISH:\n\
    setp.ne.u32 %p7, %r4, %r2; @%p7 bra LCU_CORRUPT;\n\
LCU_OK:\n\
    mov.u32 %r17, 0; st.global.u8 [%rd3], %r17; ret;\n\
LCU_BAD_CRC:\n\
    mov.u32 %r17, 1; st.global.u8 [%rd3], %r17; ret;\n\
LCU_CORRUPT:\n\
    mov.u32 %r17, 2; st.global.u8 [%rd3], %r17; ret;\n\
}\n\
.visible .entry ramshared_batch_verify_and_unpack_kernel(\n\
    .param .u64 param_slab_base_ptr, .param .u64 param_slab_pool_bytes,\n\
    .param .u64 param_offsets_ptr, .param .u64 param_src_lens_ptr,\n\
    .param .u64 param_expected_crcs_ptr, .param .u64 param_encodings_ptr,\n\
    .param .u64 param_dst_batch_ptr, .param .u32 param_dst_cap,\n\
    .param .u64 param_statuses_ptr\n\
) {\n\
    .reg .pred %p<10>; .reg .u32 %r<26>; .reg .u64 %rd<16>;\n\
    mov.u32 %r22, %tid.x; setp.ne.u32 %p1, %r22, 0; @%p1 bra LBU_RET;\n\
    mov.u32 %r23, %ctaid.x;\n\
    cvt.u64.u32 %rd4, %r23; ld.param.u64 %rd5, [param_statuses_ptr];\n\
    add.u64 %rd3, %rd5, %rd4;\n\
    mul.wide.u32 %rd6, %r23, 8; ld.param.u64 %rd7, [param_offsets_ptr];\n\
    add.u64 %rd7, %rd7, %rd6; ld.global.u64 %rd10, [%rd7];\n\
    mul.wide.u32 %rd7, %r23, 4;\n\
    ld.param.u64 %rd8, [param_src_lens_ptr]; add.u64 %rd8, %rd8, %rd7; ld.global.u32 %r1, [%rd8];\n\
    ld.param.u64 %rd8, [param_expected_crcs_ptr]; add.u64 %rd8, %rd8, %rd7; ld.global.u32 %r20, [%rd8];\n\
    ld.param.u64 %rd8, [param_encodings_ptr]; add.u64 %rd8, %rd8, %rd7; ld.global.u32 %r21, [%rd8];\n\
    ld.param.u32 %r2, [param_dst_cap];\n\
    setp.eq.u32 %p1, %r1, 0; @%p1 bra LBU_INVALID;\n\
    setp.gt.u32 %p1, %r1, %r2; @%p1 bra LBU_INVALID;\n\
    ld.param.u64 %rd8, [param_slab_pool_bytes];\n\
    setp.ge.u64 %p1, %rd10, %rd8; @%p1 bra LBU_INVALID;\n\
    cvt.u64.u32 %rd9, %r1; add.u64 %rd11, %rd10, %rd9;\n\
    setp.gt.u64 %p1, %rd11, %rd8; @%p1 bra LBU_INVALID;\n\
    ld.param.u64 %rd1, [param_slab_base_ptr]; add.u64 %rd1, %rd1, %rd10;\n\
    mul.wide.u32 %rd6, %r23, %r2; ld.param.u64 %rd5, [param_dst_batch_ptr];\n\
    add.u64 %rd2, %rd5, %rd6;\n\
    mov.u32 %r11, 0xffffffff; mov.u32 %r12, 0;\n\
LBU_CRC_BYTE:\n\
    setp.ge.u32 %p1, %r12, %r1; @%p1 bra LBU_CRC_DONE;\n\
    cvt.u64.u32 %rd4, %r12; add.u64 %rd5, %rd1, %rd4;\n\
    ld.global.u8 %r13, [%rd5]; xor.b32 %r11, %r11, %r13; mov.u32 %r14, 0;\n\
LBU_CRC_BIT:\n\
    setp.ge.u32 %p2, %r14, 8; @%p2 bra LBU_CRC_BIT_DONE;\n\
    and.b32 %r15, %r11, 1; shr.u32 %r16, %r11, 1;\n\
    setp.ne.u32 %p3, %r15, 0; xor.b32 %r17, %r16, 0xedb88320;\n\
    selp.b32 %r11, %r17, %r16, %p3; add.u32 %r14, %r14, 1; bra LBU_CRC_BIT;\n\
LBU_CRC_BIT_DONE:\n\
    add.u32 %r12, %r12, 1; bra LBU_CRC_BYTE;\n\
LBU_CRC_DONE:\n\
    not.b32 %r11, %r11; setp.ne.u32 %p4, %r11, %r20; @%p4 bra LBU_BAD_CRC;\n\
    setp.eq.u32 %p4, %r21, 1; @%p4 bra LBU_RAW;\n\
    setp.ne.u32 %p4, %r21, 2; @%p4 bra LBU_CORRUPT;\n\
    mov.u32 %r3, 0; mov.u32 %r4, 0;\n\
LBU_TOKEN:\n\
    setp.ge.u32 %p1, %r3, %r1; @%p1 bra LBU_FINISH;\n\
    cvt.u64.u32 %rd4, %r3; add.u64 %rd5, %rd1, %rd4;\n\
    ld.global.u8 %r5, [%rd5]; add.u32 %r3, %r3, 1;\n\
    shr.u32 %r6, %r5, 4; setp.ne.u32 %p2, %r6, 15; @%p2 bra LBU_LIT_READY;\n\
LBU_LIT_EXT:\n\
    setp.ge.u32 %p2, %r3, %r1; @%p2 bra LBU_CORRUPT;\n\
    cvt.u64.u32 %rd4, %r3; add.u64 %rd5, %rd1, %rd4;\n\
    ld.global.u8 %r18, [%rd5]; add.u32 %r3, %r3, 1; add.u32 %r6, %r6, %r18;\n\
    setp.eq.u32 %p2, %r18, 255; @%p2 bra LBU_LIT_EXT;\n\
LBU_LIT_READY:\n\
    add.u32 %r7, %r3, %r6; setp.gt.u32 %p2, %r7, %r1; @%p2 bra LBU_CORRUPT;\n\
    add.u32 %r8, %r4, %r6; setp.gt.u32 %p3, %r8, %r2; @%p3 bra LBU_CORRUPT;\n\
    mov.u32 %r9, 0;\n\
LBU_LIT_COPY:\n\
    setp.ge.u32 %p4, %r9, %r6; @%p4 bra LBU_AFTER_LIT;\n\
    cvt.u64.u32 %rd6, %r3; add.u64 %rd6, %rd1, %rd6; ld.global.u8 %r10, [%rd6];\n\
    cvt.u64.u32 %rd7, %r4; add.u64 %rd7, %rd2, %rd7; st.global.u8 [%rd7], %r10;\n\
    add.u32 %r3, %r3, 1; add.u32 %r4, %r4, 1; add.u32 %r9, %r9, 1; bra LBU_LIT_COPY;\n\
LBU_AFTER_LIT:\n\
    setp.ge.u32 %p5, %r3, %r1; @%p5 bra LBU_FINISH;\n\
    add.u32 %r11, %r3, 2; setp.gt.u32 %p6, %r11, %r1; @%p6 bra LBU_CORRUPT;\n\
    cvt.u64.u32 %rd6, %r3; add.u64 %rd6, %rd1, %rd6;\n\
    ld.global.u8 %r12, [%rd6]; ld.global.u8 %r13, [%rd6+1];\n\
    shl.b32 %r13, %r13, 8; or.b32 %r12, %r12, %r13; add.u32 %r3, %r3, 2;\n\
    setp.eq.u32 %p7, %r12, 0; @%p7 bra LBU_CORRUPT;\n\
    setp.gt.u32 %p7, %r12, %r4; @%p7 bra LBU_CORRUPT;\n\
    and.b32 %r14, %r5, 15; setp.ne.u32 %p7, %r14, 15; @%p7 bra LBU_MATCH_READY;\n\
LBU_MATCH_EXT:\n\
    setp.ge.u32 %p7, %r3, %r1; @%p7 bra LBU_CORRUPT;\n\
    cvt.u64.u32 %rd6, %r3; add.u64 %rd6, %rd1, %rd6;\n\
    ld.global.u8 %r19, [%rd6]; add.u32 %r3, %r3, 1; add.u32 %r14, %r14, %r19;\n\
    setp.eq.u32 %p7, %r19, 255; @%p7 bra LBU_MATCH_EXT;\n\
LBU_MATCH_READY:\n\
    add.u32 %r14, %r14, 4; add.u32 %r15, %r4, %r14;\n\
    setp.gt.u32 %p7, %r15, %r2; @%p7 bra LBU_CORRUPT;\n\
    mov.u32 %r9, 0;\n\
LBU_MATCH_COPY:\n\
    setp.ge.u32 %p4, %r9, %r14; @%p4 bra LBU_TOKEN;\n\
    sub.u32 %r16, %r4, %r12; cvt.u64.u32 %rd6, %r16; add.u64 %rd6, %rd2, %rd6;\n\
    ld.global.u8 %r10, [%rd6]; cvt.u64.u32 %rd7, %r4; add.u64 %rd7, %rd2, %rd7;\n\
    st.global.u8 [%rd7], %r10; add.u32 %r4, %r4, 1; add.u32 %r9, %r9, 1; bra LBU_MATCH_COPY;\n\
LBU_RAW:\n\
    setp.ne.u32 %p4, %r1, %r2; @%p4 bra LBU_CORRUPT;\n\
    mov.u32 %r9, 0;\n\
LBU_RAW_LOOP:\n\
    setp.ge.u32 %p4, %r9, %r1; @%p4 bra LBU_OK;\n\
    cvt.u64.u32 %rd6, %r9; add.u64 %rd6, %rd1, %rd6; ld.global.u8 %r10, [%rd6];\n\
    cvt.u64.u32 %rd7, %r9; add.u64 %rd7, %rd2, %rd7; st.global.u8 [%rd7], %r10;\n\
    add.u32 %r9, %r9, 1; bra LBU_RAW_LOOP;\n\
LBU_FINISH:\n\
    setp.ne.u32 %p7, %r4, %r2; @%p7 bra LBU_CORRUPT;\n\
LBU_OK:\n\
    mov.u32 %r17, 0; st.global.u8 [%rd3], %r17; ret;\n\
LBU_BAD_CRC:\n\
    mov.u32 %r17, 1; st.global.u8 [%rd3], %r17; ret;\n\
LBU_CORRUPT:\n\
    mov.u32 %r17, 2; st.global.u8 [%rd3], %r17; ret;\n\
LBU_INVALID:\n\
    mov.u32 %r17, 3; st.global.u8 [%rd3], %r17;\n\
LBU_RET:\n\
    ret;\n\
}\n\
.visible .entry ramshared_batch_lz4_compress_crc32_kernel(\n\
    .param .u64 param_src_batch_ptr, .param .u64 param_dst_batch_ptr,\n\
    .param .u64 param_hash_batch_ptr, .param .u64 param_out_lens_ptr,\n\
    .param .u64 param_out_crcs_ptr, .param .u64 param_out_encs_ptr\n\
) {\n\
    .reg .pred %p<8>; .reg .u32 %r<34>; .reg .u64 %rd<16>;\n\
    mov.u32 %r0, %tid.x; setp.ne.u32 %p0, %r0, 0; @%p0 bra LBC_EXIT;\n\
    mov.u32 %r1, %ctaid.x; mul.wide.u32 %rd4, %r1, 4096;\n\
    ld.param.u64 %rd1, [param_src_batch_ptr]; add.u64 %rd1, %rd1, %rd4;\n\
    ld.param.u64 %rd2, [param_dst_batch_ptr]; add.u64 %rd2, %rd2, %rd4;\n\
    ld.param.u64 %rd3, [param_hash_batch_ptr]; add.u64 %rd3, %rd3, %rd4;\n\
    mov.u32 %r2, 0; mov.u32 %r3, 0xffffffff;\n\
LBC_INIT_HASH:\n\
    setp.ge.u32 %p1, %r2, 1024; @%p1 bra LBC_HASH_DONE;\n\
    mul.wide.u32 %rd5, %r2, 4; add.u64 %rd6, %rd3, %rd5; st.global.u32 [%rd6], %r3;\n\
    add.u32 %r2, %r2, 1; bra LBC_INIT_HASH;\n\
LBC_HASH_DONE:\n\
    mov.u32 %r4, 0; mov.u32 %r5, 0; mov.u32 %r6, 0;\n\
LBC_MAIN:\n\
    setp.ge.u32 %p1, %r5, 4084; @%p1 bra LBC_LAST_LITS;\n\
    cvt.u64.u32 %rd5, %r5; add.u64 %rd5, %rd1, %rd5;\n\
    ld.global.u8 %r7, [%rd5]; ld.global.u8 %r8, [%rd5+1];\n\
    ld.global.u8 %r9, [%rd5+2]; ld.global.u8 %r10, [%rd5+3];\n\
    shl.b32 %r8, %r8, 8; or.b32 %r7, %r7, %r8;\n\
    shl.b32 %r9, %r9, 16; or.b32 %r7, %r7, %r9;\n\
    shl.b32 %r10, %r10, 24; or.b32 %r7, %r7, %r10;\n\
    mul.lo.u32 %r11, %r7, 0x9e3779b1; shr.u32 %r11, %r11, 22; and.b32 %r11, %r11, 1023;\n\
    mul.wide.u32 %rd6, %r11, 4; add.u64 %rd6, %rd3, %rd6;\n\
    ld.global.u32 %r12, [%rd6]; st.global.u32 [%rd6], %r5;\n\
    setp.ge.u32 %p1, %r12, %r5; @%p1 bra LBC_STEP_IP;\n\
    cvt.u64.u32 %rd5, %r12; add.u64 %rd5, %rd1, %rd5;\n\
    ld.global.u8 %r8, [%rd5]; ld.global.u8 %r9, [%rd5+1];\n\
    ld.global.u8 %r10, [%rd5+2]; ld.global.u8 %r13, [%rd5+3];\n\
    shl.b32 %r9, %r9, 8; or.b32 %r8, %r8, %r9;\n\
    shl.b32 %r10, %r10, 16; or.b32 %r8, %r8, %r10;\n\
    shl.b32 %r13, %r13, 24; or.b32 %r8, %r8, %r13;\n\
    setp.ne.u32 %p1, %r8, %r7; @%p1 bra LBC_STEP_IP;\n\
    add.u32 %r14, %r5, 4; add.u32 %r15, %r12, 4; mov.u32 %r16, 4;\n\
LBC_EXTEND:\n\
    setp.ge.u32 %p1, %r14, 4091; @%p1 bra LBC_EXTEND_DONE;\n\
    cvt.u64.u32 %rd5, %r14; add.u64 %rd5, %rd1, %rd5; ld.global.u8 %r8, [%rd5];\n\
    cvt.u64.u32 %rd6, %r15; add.u64 %rd6, %rd1, %rd6; ld.global.u8 %r9, [%rd6];\n\
    setp.ne.u32 %p1, %r8, %r9; @%p1 bra LBC_EXTEND_DONE;\n\
    add.u32 %r14, %r14, 1; add.u32 %r15, %r15, 1; add.u32 %r16, %r16, 1; bra LBC_EXTEND;\n\
LBC_EXTEND_DONE:\n\
    sub.u32 %r17, %r5, %r4; sub.u32 %r18, %r16, 4; sub.u32 %r19, %r5, %r12;\n\
    shr.u32 %r21, %r17, 8; add.u32 %r21, %r21, 2;\n\
    shr.u32 %r22, %r18, 8; add.u32 %r22, %r22, 2;\n\
    add.u32 %r20, %r6, 3; add.u32 %r20, %r20, %r17;\n\
    add.u32 %r20, %r20, %r21; add.u32 %r20, %r20, %r22;\n\
    setp.gt.u32 %p1, %r20, 3071; @%p1 bra LBC_RAW;\n\
    min.u32 %r21, %r17, 15; shl.b32 %r21, %r21, 4;\n\
    min.u32 %r22, %r18, 15; or.b32 %r21, %r21, %r22;\n\
    cvt.u64.u32 %rd5, %r6; add.u64 %rd5, %rd2, %rd5; st.global.u8 [%rd5], %r21;\n\
    add.u32 %r6, %r6, 1;\n\
    setp.lt.u32 %p1, %r17, 15; @%p1 bra LBC_LIT_COPY_INIT;\n\
    sub.u32 %r21, %r17, 15;\n\
LBC_LIT_EXT_LOOP:\n\
    setp.lt.u32 %p1, %r21, 255; @%p1 bra LBC_LIT_EXT_TAIL;\n\
    cvt.u64.u32 %rd5, %r6; add.u64 %rd5, %rd2, %rd5; mov.u32 %r22, 255; st.global.u8 [%rd5], %r22;\n\
    add.u32 %r6, %r6, 1; sub.u32 %r21, %r21, 255; bra LBC_LIT_EXT_LOOP;\n\
LBC_LIT_EXT_TAIL:\n\
    cvt.u64.u32 %rd5, %r6; add.u64 %rd5, %rd2, %rd5; st.global.u8 [%rd5], %r21; add.u32 %r6, %r6, 1;\n\
LBC_LIT_COPY_INIT:\n\
    mov.u32 %r21, 0;\n\
LBC_COPY_LITS:\n\
    setp.ge.u32 %p1, %r21, %r17; @%p1 bra LBC_WRITE_OFF;\n\
    add.u32 %r22, %r4, %r21; cvt.u64.u32 %rd5, %r22; add.u64 %rd5, %rd1, %rd5; ld.global.u8 %r23, [%rd5];\n\
    cvt.u64.u32 %rd6, %r6; add.u64 %rd6, %rd2, %rd6; st.global.u8 [%rd6], %r23;\n\
    add.u32 %r6, %r6, 1; add.u32 %r21, %r21, 1; bra LBC_COPY_LITS;\n\
LBC_WRITE_OFF:\n\
    and.b32 %r21, %r19, 255; cvt.u64.u32 %rd5, %r6; add.u64 %rd5, %rd2, %rd5; st.global.u8 [%rd5], %r21;\n\
    shr.u32 %r21, %r19, 8; and.b32 %r21, %r21, 255; st.global.u8 [%rd5+1], %r21; add.u32 %r6, %r6, 2;\n\
    setp.lt.u32 %p1, %r18, 15; @%p1 bra LBC_ADVANCE;\n\
    sub.u32 %r21, %r18, 15;\n\
LBC_ML_EXT_LOOP:\n\
    setp.lt.u32 %p1, %r21, 255; @%p1 bra LBC_ML_EXT_TAIL;\n\
    cvt.u64.u32 %rd5, %r6; add.u64 %rd5, %rd2, %rd5; mov.u32 %r22, 255; st.global.u8 [%rd5], %r22;\n\
    add.u32 %r6, %r6, 1; sub.u32 %r21, %r21, 255; bra LBC_ML_EXT_LOOP;\n\
LBC_ML_EXT_TAIL:\n\
    cvt.u64.u32 %rd5, %r6; add.u64 %rd5, %rd2, %rd5; st.global.u8 [%rd5], %r21; add.u32 %r6, %r6, 1;\n\
LBC_ADVANCE:\n\
    add.u32 %r5, %r5, %r16; mov.u32 %r4, %r5; bra LBC_MAIN;\n\
LBC_STEP_IP:\n\
    add.u32 %r5, %r5, 1; bra LBC_MAIN;\n\
LBC_LAST_LITS:\n\
    mov.u32 %r21, 4096; sub.u32 %r17, %r21, %r4;\n\
    shr.u32 %r22, %r17, 8; add.u32 %r20, %r6, 3; add.u32 %r20, %r20, %r17; add.u32 %r20, %r20, %r22;\n\
    setp.gt.u32 %p1, %r20, 3071; @%p1 bra LBC_RAW;\n\
    min.u32 %r21, %r17, 15; shl.b32 %r21, %r21, 4;\n\
    cvt.u64.u32 %rd5, %r6; add.u64 %rd5, %rd2, %rd5; st.global.u8 [%rd5], %r21; add.u32 %r6, %r6, 1;\n\
    setp.lt.u32 %p1, %r17, 15; @%p1 bra LBC_LAST_COPY_INIT;\n\
    sub.u32 %r21, %r17, 15;\n\
LBC_LAST_EXT_LOOP:\n\
    setp.lt.u32 %p1, %r21, 255; @%p1 bra LBC_LAST_EXT_TAIL;\n\
    cvt.u64.u32 %rd5, %r6; add.u64 %rd5, %rd2, %rd5; mov.u32 %r22, 255; st.global.u8 [%rd5], %r22;\n\
    add.u32 %r6, %r6, 1; sub.u32 %r21, %r21, 255; bra LBC_LAST_EXT_LOOP;\n\
LBC_LAST_EXT_TAIL:\n\
    cvt.u64.u32 %rd5, %r6; add.u64 %rd5, %rd2, %rd5; st.global.u8 [%rd5], %r21; add.u32 %r6, %r6, 1;\n\
LBC_LAST_COPY_INIT:\n\
    mov.u32 %r21, 0;\n\
LBC_LAST_COPY:\n\
    setp.ge.u32 %p1, %r21, %r17; @%p1 bra LBC_COMP_OK;\n\
    add.u32 %r22, %r4, %r21; cvt.u64.u32 %rd5, %r22; add.u64 %rd5, %rd1, %rd5; ld.global.u8 %r23, [%rd5];\n\
    cvt.u64.u32 %rd6, %r6; add.u64 %rd6, %rd2, %rd6; st.global.u8 [%rd6], %r23;\n\
    add.u32 %r6, %r6, 1; add.u32 %r21, %r21, 1; bra LBC_LAST_COPY;\n\
LBC_COMP_OK:\n\
    mov.u32 %r24, %r6; mov.u32 %r25, 2; bra LBC_CRC;\n\
LBC_RAW:\n\
    mov.u32 %r21, 0;\n\
LBC_RAW_COPY:\n\
    setp.ge.u32 %p1, %r21, 4096; @%p1 bra LBC_RAW_DONE;\n\
    cvt.u64.u32 %rd5, %r21; add.u64 %rd6, %rd1, %rd5; ld.global.u8 %r23, [%rd6];\n\
    add.u64 %rd7, %rd2, %rd5; st.global.u8 [%rd7], %r23;\n\
    add.u32 %r21, %r21, 1; bra LBC_RAW_COPY;\n\
LBC_RAW_DONE:\n\
    mov.u32 %r24, 4096; mov.u32 %r25, 1;\n\
LBC_CRC:\n\
    mov.u32 %r26, 0xffffffff; mov.u32 %r27, 0;\n\
LBC_CRC_BYTE:\n\
    setp.ge.u32 %p1, %r27, %r24; @%p1 bra LBC_CRC_DONE;\n\
    cvt.u64.u32 %rd5, %r27; add.u64 %rd6, %rd2, %rd5;\n\
    ld.global.u8 %r28, [%rd6]; xor.b32 %r26, %r26, %r28; mov.u32 %r29, 0;\n\
LBC_CRC_BIT:\n\
    setp.ge.u32 %p2, %r29, 8; @%p2 bra LBC_CRC_BIT_DONE;\n\
    and.b32 %r30, %r26, 1; shr.u32 %r31, %r26, 1;\n\
    setp.ne.u32 %p3, %r30, 0; xor.b32 %r32, %r31, 0xedb88320;\n\
    selp.b32 %r26, %r32, %r31, %p3; add.u32 %r29, %r29, 1; bra LBC_CRC_BIT;\n\
LBC_CRC_BIT_DONE:\n\
    add.u32 %r27, %r27, 1; bra LBC_CRC_BYTE;\n\
LBC_CRC_DONE:\n\
    not.b32 %r26, %r26;\n\
    mul.wide.u32 %rd8, %r1, 4;\n\
    ld.param.u64 %rd9, [param_out_lens_ptr]; add.u64 %rd9, %rd9, %rd8; st.global.u32 [%rd9], %r24;\n\
    ld.param.u64 %rd9, [param_out_crcs_ptr]; add.u64 %rd9, %rd9, %rd8; st.global.u32 [%rd9], %r26;\n\
    ld.param.u64 %rd9, [param_out_encs_ptr]; add.u64 %rd9, %rd9, %rd8; st.global.u32 [%rd9], %r25;\n\
LBC_EXIT:\n\
    ret;\n\
}\0";

/// Validates the embedded PTX header, NUL terminator, and required entry points.
#[must_use]
pub fn validate_embedded_ptx_image(ptx: &[u8]) -> bool {
    let Ok(text) = std::str::from_utf8(ptx) else {
        return false;
    };
    text.starts_with(".version 7.0\n.target sm_50\n.address_size 64\n")
        && text.ends_with('\0')
        && text.contains(".visible .entry ramshared_crc32_verify_kernel(")
        && text.contains(".visible .entry ramshared_lz4_block_codec_kernel(")
        && text.contains(".visible .entry ramshared_crc32_and_lz4_unpack_kernel(")
        && text.contains(".visible .entry ramshared_batch_verify_and_unpack_kernel(")
        && text.contains(".visible .entry ramshared_batch_lz4_compress_crc32_kernel(")
        && text.contains("0xedb88320")
        && text.contains("L_LZ4_TOKEN_LOOP:")
        && text.contains("L_LZ4_LIT_EXT:")
        && text.contains("L_LZ4_MATCH_EXT:")
        && text.contains("L_LZ4_MATCH_COPY:")
        && text.contains("LBU_CRC_BYTE:")
        && text.contains("LBC_MAIN:")
}
