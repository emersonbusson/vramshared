// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-tier/src/lib.rs
//! ramshared-tier — Orchestration of the RamShared swap cascade in WSL2.
//!
//! §9 (DEMOTE/residency).
//!
//! The cascade is priority-ordered using `swapon` settings, validated empirically in Phase 0
//! (`Documentation/architecture/three-tier-cascade.rst`):
//!
//! ```text
//! memory pressure → zram (HOT, prio 200) → VRAM (COLD, prio 100) → VHDX (prio < 100)
//! ```
//!
//! This crate contains **pure, testable logic** (no root access, no filesystem I/O, no FFI):
//! priority constants, cascade order validation, and the DEMOTE safety net invariant
//! (finding A1 from the v3 audit).
#![forbid(unsafe_code)]

pub mod cascade;
pub mod drain_safety;
pub mod memory_headroom;
pub mod n3_state;
pub mod nbd_readiness;
pub mod priority;

pub use cascade::{SafetyNet, Tier, vram_safety_net};
pub use drain_safety::{
    DRAIN_MARGIN_BYTES, DRAIN_OBSERVATION_MAX_BYTES, DrainBudget, DrainDecision, DrainMemory,
    HostDrainMemory, MIN_DRAIN_RESERVE_BYTES, meminfo_counter_kib, parse_free_bytes,
};
pub use priority::{OrderError, TierPriorities, VRAM_PRIO, ZRAM_PRIO, validate_order};
