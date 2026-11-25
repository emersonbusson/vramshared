// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-vram/src/slab_pool/tests.rs
#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;

#[test]
fn eviction_observation_reports_only_its_bounded_interval() {
    let mut pool = VramSlabPool::new(1).unwrap();
    for _ in 0..SlabClass::Raw4096.slots_per_frame() {
        pool.alloc_sub_slot(SlabClass::Raw4096).unwrap();
    }
    pool.begin_eviction_observation();
    pool.alloc_sub_slot(SlabClass::Raw4096).unwrap();
    assert_eq!(pool.finish_eviction_observation(), vec![0]);
    assert!(pool.finish_eviction_observation().is_empty());
    pool.begin_eviction_observation();
    assert!(pool.finish_eviction_observation().is_empty());
}

#[test]
fn frame_epoch_cleanup_preserves_other_frames() {
    let mut pool = VramSlabPool::new(2).unwrap();
    pool.slot_epochs.insert((0, 0), 7);
    pool.slot_epochs.insert((0, 127), 8);
    pool.slot_epochs.insert((1, 0), 9);
    pool.purge_frame_epochs(0);
    assert!(!pool.slot_epochs.contains_key(&(0, 0)));
    assert!(!pool.slot_epochs.contains_key(&(0, 127)));
    assert_eq!(pool.slot_epochs.get(&(1, 0)), Some(&9));
}

#[test]
fn fixed_metadata_lower_bound_scales_with_frame_count() {
    let small = VramSlabPool::new(1).unwrap();
    let mut large = VramSlabPool::new(4096).unwrap();
    let frame_bytes = std::mem::size_of::<Frame>() as u64;
    assert!(small.known_fixed_metadata_bytes() >= frame_bytes);
    assert!(large.known_fixed_metadata_bytes() >= frame_bytes * 4096);
    assert!(large.known_fixed_metadata_bytes() > small.known_fixed_metadata_bytes());
    assert!(VramSlabPool::per_frame_fixed_metadata_bytes() >= std::mem::size_of::<Frame>());

    let mut ticket = large.alloc_sub_slot(SlabClass::Raw4096).unwrap();
    large
        .commit(&mut ticket, SlotEncoding::Raw, 4096, 1)
        .unwrap();
    assert_eq!(large.slot_epochs_len(), 0);
    assert_eq!(large.evict_lru_frame(), Some(ticket.frame_idx));
    assert_eq!(large.active_frames(), 0);
    assert_eq!(large.logical_pages_cached(), 0);
    assert!(large.known_total_metadata_bytes() >= large.known_fixed_metadata_bytes());
}

#[test]
fn slab_pool_subslot_alloc_free_and_alignment_invariants() {
    for class in SlabClass::ALL {
        let mut pool = VramSlabPool::new(1).unwrap();
        let mut tickets = Vec::new();
        for _ in 0..class.slots_per_frame() {
            let ticket = pool.alloc_sub_slot(class).unwrap();
            assert_eq!(ticket.vram_byte_offset().unwrap() % SLOT_ALIGNMENT_BYTES, 0);
            assert!(ticket.vram_byte_offset().unwrap() < FRAME_BYTES as u64);
            tickets.push(ticket);
        }
        let replacement = pool.alloc_sub_slot(class).unwrap();
        assert_eq!(pool.evictions(), 1);
        for ticket in &tickets {
            assert!(!pool.invalidate(ticket));
        }
        assert!(pool.invalidate(&replacement));
        assert_eq!(pool.logical_pages_cached(), 0);
        assert_eq!(pool.active_frames(), 0);
    }
}

#[test]
fn slab_pool_lru_frame_eviction_invalidates_all_subslots_via_epoch() {
    let mut pool = VramSlabPool::new(2).unwrap();
    let mut tickets = Vec::new();
    for _ in 0..(2usize * usize::from(SlabClass::B512.slots_per_frame())) {
        tickets.push(pool.alloc_sub_slot(SlabClass::B512).unwrap());
    }
    let evicted_frame = tickets[0].frame_idx;
    let replacement = pool.alloc_sub_slot(SlabClass::B2048).unwrap();
    assert_eq!(replacement.class, SlabClass::B2048);
    assert_eq!(pool.evictions(), 1);
    for ticket in tickets
        .iter()
        .filter(|ticket| ticket.frame_idx == evicted_frame)
    {
        assert!(matches!(pool.lookup(ticket), Err(SlabError::InvalidTicket)));
    }
    assert!(pool.lookup(&replacement).is_ok());
    let follow_up_b512 = pool.alloc_sub_slot(SlabClass::B512).unwrap();
    assert_eq!(follow_up_b512.class, SlabClass::B512);
    assert!(pool.lookup(&follow_up_b512).is_ok());
}

#[test]
fn slab_pool_partial_write_invalidates_ticket_idempotently() {
    let mut pool = VramSlabPool::new(1).unwrap();
    let mut ticket = pool.alloc_sub_slot(SlabClass::B512).unwrap();
    pool.commit(&mut ticket, SlotEncoding::Lz4, 400, 7).unwrap();
    assert_eq!(pool.logical_pages_cached(), 1);
    assert!(pool.invalidate(&ticket));
    assert!(!pool.invalidate(&ticket));
    let mut replacement = pool.alloc_sub_slot(SlabClass::B512).unwrap();
    pool.commit(&mut replacement, SlotEncoding::Lz4, 400, 7)
        .unwrap();
    assert!(!pool.invalidate(&ticket));
    assert!(pool.lookup(&replacement).is_ok());
    assert_eq!(pool.logical_pages_cached(), 1);
}

#[test]
fn slab_pool_achieves_target_density_on_mixed_corpus() {
    let mut pool = VramSlabPool::new(256).unwrap();
    let mix = [
        (SlabClass::B512, 1_638usize),
        (SlabClass::B1024, 1_229usize),
        (SlabClass::B2048, 614usize),
        (SlabClass::Raw4096, 615usize),
    ];
    for (class, count) in mix {
        for _ in 0..count {
            let mut ticket = pool.alloc_sub_slot(class).unwrap();
            let len = match class {
                SlabClass::Raw4096 => 4096,
                _ => class.stride_bytes() as u16,
            };
            let encoding = if class == SlabClass::Raw4096 {
                SlotEncoding::Raw
            } else {
                SlotEncoding::Lz4
            };
            pool.commit(&mut ticket, encoding, len, 0).unwrap();
        }
    }
    let ratio = pool.effective_density_ratio().unwrap();
    assert!(ratio >= 1.60, "density ratio was {ratio}");
}

#[test]
fn slab_ticket_rejects_invalid_class_and_encoding_combinations() {
    let mut pool = VramSlabPool::new(1).unwrap();
    let mut ticket = pool.alloc_sub_slot(SlabClass::B512).unwrap();
    assert!(matches!(
        pool.commit(&mut ticket, SlotEncoding::Raw, 4096, 0),
        Err(SlabError::InvalidTicket)
    ));
    let mut raw = pool.alloc_sub_slot(SlabClass::Raw4096).unwrap();
    assert!(matches!(
        pool.commit(&mut raw, SlotEncoding::Lz4, 3072, 0),
        Err(SlabError::InvalidTicket)
    ));
    assert!(matches!(
        pool.commit(&mut raw, SlotEncoding::Raw, 4095, 0),
        Err(SlabError::InvalidTicket)
    ));
}

#[test]
fn slab_pool_rejects_zero_frames_and_unchecked_offsets() {
    assert!(matches!(
        VramSlabPool::new(0),
        Err(SlabError::InvalidFrameCount)
    ));
    let ticket = VramSlabTicket {
        frame_idx: u32::MAX,
        sub_slot: 0,
        class: SlabClass::Raw4096,
        encoding: SlotEncoding::Raw,
        payload_len: 4096,
        payload_crc32: 0,
        epoch: 1,
    };
    assert_eq!(ticket.vram_byte_offset(), Some(u32::MAX as u64 * 65_536));
    assert!(ticket.end_offset().is_some());
    let pool = VramSlabPool::new(1).unwrap();
    assert!(matches!(
        pool.lookup(&ticket),
        Err(SlabError::FrameOutOfRange)
    ));
}

#[test]
fn slab_pool_commit_failure_releases_the_reserved_slot() {
    let mut pool = VramSlabPool::new(1).unwrap();
    let mut ticket = pool.alloc_sub_slot(SlabClass::B512).unwrap();
    assert!(
        pool.commit(&mut ticket, SlotEncoding::Raw, 4096, 0)
            .is_err()
    );
    assert_eq!(pool.logical_pages_cached(), 0);
    let mut replacement = pool.alloc_sub_slot(SlabClass::B512).unwrap();
    pool.commit(&mut replacement, SlotEncoding::Lz4, 100, 1)
        .unwrap();
    assert_eq!(pool.evictions(), 0);
    assert_eq!(pool.logical_pages_cached(), 1);
}

#[test]
fn slab_pool_lookup_and_touch_promotes_a_read_hot_frame() {
    let mut pool = VramSlabPool::new(2).unwrap();
    let first = pool.alloc_sub_slot(SlabClass::B512).unwrap();
    for _ in 1..SlabClass::B512.slots_per_frame() {
        let _ = pool.alloc_sub_slot(SlabClass::B512).unwrap();
    }
    let second = pool.alloc_sub_slot(SlabClass::B512).unwrap();
    assert_ne!(first.frame_idx, second.frame_idx);
    pool.lookup_and_touch(&first).unwrap();
    let replacement = pool.alloc_sub_slot(SlabClass::B2048).unwrap();
    assert_eq!(pool.evictions(), 1);
    assert!(pool.lookup(&first).is_ok());
    assert!(matches!(
        pool.lookup(&second),
        Err(SlabError::InvalidTicket)
    ));
    assert!(pool.lookup(&replacement).is_ok());
    let follow_up_b512 = pool.alloc_sub_slot(SlabClass::B512).unwrap();
    assert_eq!(follow_up_b512.class, SlabClass::B512);
    assert!(pool.lookup(&follow_up_b512).is_ok());
}

#[test]
fn slab_class_selection_matches_dt4_boundaries() {
    assert_eq!(SlabClass::select_for_len(0), SlabClass::B512);
    assert_eq!(SlabClass::select_for_len(512), SlabClass::B512);
    assert_eq!(SlabClass::select_for_len(513), SlabClass::B1024);
    assert_eq!(SlabClass::select_for_len(1024), SlabClass::B1024);
    assert_eq!(SlabClass::select_for_len(1025), SlabClass::B2048);
    assert_eq!(SlabClass::select_for_len(2048), SlabClass::B2048);
    assert_eq!(SlabClass::select_for_len(2049), SlabClass::B3072);
    assert_eq!(SlabClass::select_for_len(3071), SlabClass::B3072);
    assert_eq!(SlabClass::select_for_len(3072), SlabClass::Raw4096);
    assert_eq!(SlabClass::from_index(4), Some(SlabClass::Raw4096));
    assert_eq!(SlabClass::from_index(5), None);
}
