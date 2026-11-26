// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-vram/tests/public_contracts.rs
//! Public consumer contracts survive internal responsibility extraction.
#![allow(clippy::expect_used)]
use ramshared_vram::{
    GPU_BUDGET_MAX_AGE_MS, GpuAdapterIdentity, GpuBudgetSnapshot, GpuBudgetSource,
    GpuBudgetTelemetry, GpuUtilizationSnapshot, GpuUtilizationTelemetry, SlabClass, SlotEncoding,
    VramSlabTicket,
};
use std::time::Instant;

fn identity() -> GpuAdapterIdentity {
    GpuAdapterIdentity {
        node_mask: None,
        backend: "cuda".into(),
        key: "00112233445566778899aabbccddeeff".into(),
        luid: None,
    }
}

#[test]
fn public_budget_wire_shape_and_trust_remain_stable() {
    let snapshot = GpuBudgetSnapshot {
        adapter: Some(identity()),
        total_bytes: Some(8 << 30),
        budget_bytes: 8 << 30,
        used_bytes: 1 << 30,
        source: GpuBudgetSource::DriverReported,
        sampled_at: Instant::now(),
    };
    let telemetry = GpuBudgetTelemetry::from_snapshot(&snapshot, 1_000);
    let encoded = serde_json::to_string(&telemetry).expect("published budget");
    assert_eq!(
        encoded,
        r#"{"schema_version":1,"adapter":{"backend":"cuda","key":"00112233445566778899aabbccddeeff","luid":null},"total_bytes":8589934592,"budget_bytes":8589934592,"used_bytes":1073741824,"available_bytes":7516192768,"source":"driver_reported","sampled_at_unix_ms":1000}"#
    );
    let decoded: GpuBudgetTelemetry = serde_json::from_str(&encoded).expect("same schema");
    assert_eq!(decoded, telemetry);
    assert_eq!(
        decoded.trusted_available_at(1_000, GPU_BUDGET_MAX_AGE_MS),
        Some(7 << 30)
    );
    assert_eq!(
        decoded.trusted_available_at(999, GPU_BUDGET_MAX_AGE_MS),
        None
    );
    assert_eq!(
        decoded.trusted_available_at(1_001 + GPU_BUDGET_MAX_AGE_MS, GPU_BUDGET_MAX_AGE_MS),
        None
    );
    let estimate = GpuBudgetTelemetry {
        source: GpuBudgetSource::ProviderLocalEstimate,
        ..decoded
    };
    assert!(estimate.is_well_formed());
    assert_eq!(
        estimate.trusted_available_at(1_000, GPU_BUDGET_MAX_AGE_MS),
        None
    );
    assert_eq!(
        identity().resource_profile_key().as_deref(),
        Some("nvidia-uuid:gpu-00112233-4455-6677-8899-aabbccddeeff")
    );
}

#[test]
fn public_utilization_wire_shape_preserves_measured_idle() {
    let sample = GpuUtilizationTelemetry::from_snapshot(
        &GpuUtilizationSnapshot {
            gpu_pct: 0,
            memory_pct: 0,
            encoder_pct: None,
            decoder_pct: None,
        },
        1_000,
    );
    assert_eq!(
        serde_json::to_string(&sample).expect("utilization"),
        r#"{"schema_version":1,"gpu_pct":0,"memory_pct":0,"sampled_at_unix_ms":1000}"#
    );
    assert!(sample.trusted_at(1_000, GPU_BUDGET_MAX_AGE_MS));
    assert!(
        GpuUtilizationTelemetry::from_published_status(
            &serde_json::json!({"gpu_utilization": null})
        )
        .is_none()
    );
    let invalid = GpuUtilizationTelemetry {
        gpu_pct: 101,
        ..sample
    };
    assert!(!invalid.trusted_at(1_000, GPU_BUDGET_MAX_AGE_MS));
}

#[test]
fn public_slab_ticket_layout_and_offsets_remain_stable() {
    let ticket = VramSlabTicket {
        frame_idx: 3,
        sub_slot: 2,
        class: SlabClass::B1024,
        encoding: SlotEncoding::Lz4,
        payload_len: 512,
        payload_crc32: 0x1234,
        epoch: 17,
    };
    let module_ticket: ramshared_vram::slab_pool::VramSlabTicket = ticket;
    assert_eq!(module_ticket.vram_byte_offset(), Some(198_656));
    assert_eq!(module_ticket.end_offset(), Some(199_680));
    assert!(module_ticket.validate().is_ok());
    assert_eq!(SlabClass::ALL.map(|class| class as u8), [0, 1, 2, 3, 4]);
    assert_eq!(SlotEncoding::Lz4 as u8, 0);
    assert_eq!(SlotEncoding::Raw as u8, 1);
    #[cfg(target_pointer_width = "64")]
    {
        assert_eq!(std::mem::size_of::<VramSlabTicket>(), 24);
        assert_eq!(std::mem::align_of::<VramSlabTicket>(), 8);
        assert_eq!(std::mem::offset_of!(VramSlabTicket, payload_crc32), 12);
        assert_eq!(std::mem::offset_of!(VramSlabTicket, epoch), 16);
    }
    let invalid = VramSlabTicket {
        sub_slot: 64,
        ..ticket
    };
    assert!(invalid.validate().is_err());
}

#[test]
fn public_lz4_golden_block_and_malformed_input_contract() {
    use ramshared_vram::device_codec::{lz4_compress_block, lz4_decompress_block};
    let page = [b'A'; 4096];
    let mut expected = vec![0x1f, b'A', 1, 0];
    expected.extend_from_slice(&[0xff; 15]);
    expected.extend_from_slice(&[0xf6, 0x50]);
    expected.extend_from_slice(&[b'A'; 5]);
    assert_eq!(
        lz4_compress_block(&page).as_deref(),
        Some(expected.as_slice())
    );
    let mut output = [0; 4096];
    lz4_decompress_block(&expected, &mut output).expect("golden overlapping match");
    assert_eq!(output, page);
    for bad in [
        &[][..],
        &[0xf0],
        &[0, 0, 0],
        &[0, 1, 0],
        &expected[..expected.len() - 1],
    ] {
        assert!(lz4_decompress_block(bad, &mut output).is_err());
    }
}

#[test]
fn unavailable_wall_clock_never_licenses_utilization() {
    for (sampled, now) in [(0, 0), (0, 1_000), (1_000, 0)] {
        let sample = GpuUtilizationTelemetry::from_snapshot(
            &GpuUtilizationSnapshot {
                gpu_pct: 0,
                memory_pct: 0,
                encoder_pct: None,
                decoder_pct: None,
            },
            sampled,
        );
        assert!(
            !sample.trusted_at(now, GPU_BUDGET_MAX_AGE_MS),
            "an unavailable clock was accepted as fresh utilization: sampled={sampled}, now={now}"
        );
    }
}

#[test]
fn default_provider_cannot_invent_driver_authority_or_utilization() {
    use ramshared_vram::{VramError, VramMemory, VramProvider};
    struct NoMemory;
    impl VramMemory for NoMemory {
        fn len(&self) -> usize {
            0
        }
        fn zero(&mut self) -> Result<(), VramError> {
            Err(VramError::Busy)
        }
        fn read_at(&self, _: u64, _: &mut [u8]) -> Result<(), VramError> {
            Err(VramError::Busy)
        }
        fn write_at(&mut self, _: u64, _: &[u8]) -> Result<(), VramError> {
            Err(VramError::Busy)
        }
    }
    struct LegacyProvider(bool);
    impl VramProvider for LegacyProvider {
        type Mem<'p> = NoMemory;
        fn alloc(&self, _: usize) -> Result<NoMemory, VramError> {
            Err(VramError::OutOfMemory)
        }
        fn mem_info(&self) -> Result<(u64, u64), VramError> {
            if self.0 {
                Err(VramError::Busy)
            } else {
                Ok((6 << 30, 8 << 30))
            }
        }
    }
    let provider = LegacyProvider(false);
    let budget = provider.budget_snapshot().expect("legacy observation");
    assert_eq!(budget.adapter, None);
    assert_eq!(budget.source, GpuBudgetSource::ProviderLocalEstimate);
    assert_eq!(budget.used_bytes, 2 << 30);
    assert!(!budget.can_admit(0));
    assert!(provider.cache_codec().is_none());
    assert!(provider.utilization_snapshot().is_none());
    assert!(matches!(
        LegacyProvider(true).budget_snapshot(),
        Err(VramError::Busy)
    ));
}

#[test]
fn adapter_node_scope_roundtrips_without_guessing() {
    for mask in [1_u32, 2, 1 << 31] {
        let body = serde_json::json!({
            "backend": "cuda", "key": identity().key,
            "luid": "00000001:00000002", "node_mask": mask,
        });
        let parsed: GpuAdapterIdentity =
            serde_json::from_value(body.clone()).expect("observed node");
        assert_eq!(serde_json::to_value(&parsed).expect("node wire"), body);
        assert!(parsed.is_stable_driver_identity());
        assert_eq!(
            parsed.wddm_physical_adapter_index(),
            Some(mask.trailing_zeros())
        );
    }
    for mask in [0_u32, 3, u32::MAX] {
        let body = serde_json::json!({
            "backend": "cuda", "key": identity().key,
            "luid": "00000001:00000002", "node_mask": mask,
        });
        let parsed: GpuAdapterIdentity = serde_json::from_value(body).expect("shape parses");
        assert!(!parsed.is_stable_driver_identity());
        assert_eq!(parsed.wddm_physical_adapter_index(), None);
    }
    let old = serde_json::json!({"backend": "cuda", "key": identity().key, "luid": null});
    let parsed: GpuAdapterIdentity =
        serde_json::from_value(old.clone()).expect("unscoped identity");
    assert_eq!(serde_json::to_value(parsed).expect("old wire"), old);
}

#[test]
fn same_luid_different_nodes_are_not_the_same_physical_memory() {
    let parse = |backend: &str, node_mask: Option<u32>| {
        serde_json::from_value::<GpuAdapterIdentity>(serde_json::json!({
            "backend": backend, "key": identity().key,
            "luid": "00000001:00000002", "node_mask": node_mask,
        }))
        .expect("node identity")
    };
    let first = parse("cuda", Some(1));
    assert!(first.matches_physical_adapter(&parse("vulkan", Some(1))));
    assert!(!first.matches_physical_adapter(&parse("vulkan", Some(2))));
    assert!(!first.matches_physical_adapter(&parse("cuda", Some(2))));
    assert!(!first.matches_physical_adapter(&parse("vulkan", None)));
}

#[test]
fn video_utilization_preserves_legacy_samples() {
    let old = serde_json::json!({
        "gpu_utilization": {
            "schema_version": 1,
            "gpu_pct": 26,
            "memory_pct": 21,
            "sampled_at_unix_ms": 42
        }
    });
    let sample = GpuUtilizationTelemetry::from_published_status(&old).expect("legacy sample");
    assert_eq!((sample.encoder_pct, sample.decoder_pct), (None, None));
    assert!(sample.trusted_at(42, GPU_BUDGET_MAX_AGE_MS));
    assert_eq!(
        serde_json::to_value(sample).expect("serialized utilization"),
        old["gpu_utilization"]
    );

    let video = GpuUtilizationTelemetry {
        encoder_pct: Some(41),
        decoder_pct: Some(0),
        ..sample
    };
    let status = serde_json::json!({"gpu_utilization": video});
    assert_eq!(
        GpuUtilizationTelemetry::from_published_status(&status),
        Some(video)
    );
    assert!(video.is_well_formed());
    assert!(
        !GpuUtilizationTelemetry {
            decoder_pct: Some(101),
            ..video
        }
        .is_well_formed()
    );
}
