// SPDX-License-Identifier: MIT
// Copyright (c) 2024-2026 Emerson Busson. All rights reserved.
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/published_budget.rs
//! The published GPU telemetry samples and the origin-plane I/O accounting.
//!
//! The daemon writes `gpu_budget`, `gpu_utilization`, `gpu_cache` and
//! `origin_cache_io` into its cache-status document. This module names that
//! document, reads it, and reduces each field to its typed, trusted sample. The
//! cascade and readiness both call it; the reduction is defined once, here.
//!
//! Adapter inventory (`total` / `free` from a provider probe) is **not** a
//! budget and is never mapped in here. Two authorities would disagree and the
//! operator could not tell which one admission used.
//!

use std::path::Path;

use ramshared_block::OriginCacheIo;
use ramshared_vram::{GpuBudgetTelemetry, GpuUtilizationTelemetry, WorkerCacheTelemetry};

/// Where the daemon publishes its cache status. The budget sample is the
/// `gpu_budget` field of that document. Declared once, here.
pub(crate) const CACHE_STATUS_FILE: &str = "/run/ramshared/cache-status.json";

/// Read and parse a published cache-status document.
///
/// Fail-closed at every step: an unreadable path, a body that is not JSON, and
/// a file this reader will not trust are all `None`, never a partial or guessed
/// document. The path is the **caller's** — production passes
/// [`CACHE_STATUS_FILE`] — so this reader cannot grow a second hardcoded
/// location and is testable without owning the runtime path.
///
/// Trust is about who may write the document, and the answer is not the
/// reader. `ramsharedd` publishes it as root and renames it into place, while
/// the readers are the unprivileged operator's tools, so the
/// [`crate::workload`] ledger check `st_uid == geteuid()` deliberately does
/// **not** transfer: it would refuse the legitimate root-owned document on
/// every unprivileged read. What must hold is that the document belongs to the
/// authority that owns the publication point — the file's owner is the
/// directory's owner, and neither is group- or world-writable. A peer user
/// cannot then substitute a document, whatever directory a future caller hands
/// this reader. Authenticity of the *body* is a separate gate and stays where
/// it belongs: [`crate::cascade::cache_status_is_current_daemon`] matches the
/// document's `daemon_instance_id` and freshness against the running daemon.
///
/// Both opens are `O_NOFOLLOW` and every trust check runs on a descriptor
/// that was held for the whole read. The publication point is opened first
/// and the document is opened **relative to that descriptor**, so the file
/// this reader stats is provably a child of the directory it is about to
/// trust; a path re-lookup after the open would be a window. An empty parent
/// (a bare relative name) is the process's own directory, which is what
/// `openat(AT_FDCWD, …)` already means.
pub(crate) fn read_status_document(path: &Path) -> Option<serde_json::Value> {
    use rustix::fs::{FileType, Mode, OFlags, fstat, open, openat};

    let parent = path.parent()?;
    let name = path.file_name()?;
    let parent = if parent.as_os_str().is_empty() {
        Path::new(".")
    } else {
        parent
    };
    let publication_point = open(
        parent,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .ok()?;
    let file = openat(
        &publication_point,
        name,
        // `NONBLOCK` so a fifo planted at the path cannot hang the open: the
        // type check below refuses it, and the flag is a no-op for a regular
        // file's reads.
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
        Mode::empty(),
    )
    .ok()?;
    let file_stat = fstat(&file).ok()?;
    let parent_stat = fstat(&publication_point).ok()?;
    if !published_document_is_trusted(
        FileType::from_raw_mode(file_stat.st_mode) == FileType::RegularFile,
        file_stat.st_mode,
        file_stat.st_uid,
        parent_stat.st_mode,
        parent_stat.st_uid,
    ) {
        return None;
    }
    let limit = ramshared_block::cache_group::MAX_CACHE_STATUS_BYTES;
    if file_stat.st_size < 0 || file_stat.st_size as u64 > limit as u64 {
        return None;
    }
    read_bounded_document(std::fs::File::from(file))
}

fn read_bounded_document(reader: impl std::io::Read) -> Option<serde_json::Value> {
    use std::io::Read;
    let limit = ramshared_block::cache_group::MAX_CACHE_STATUS_BYTES;
    let mut text = Vec::new();
    reader.take(limit as u64 + 1).read_to_end(&mut text).ok()?;
    (text.len() <= limit)
        .then(|| serde_json::from_slice(&text).ok())
        .flatten()
}

/// Whether a published document is one this reader will open at all.
///
/// Three rules hold together. The document is a regular file — not a
/// directory, a device or a fifo. Only its owner may write it, and only the
/// owner of the publication point may write that; a directory a peer can write
/// is not a publication point. And the document's owner is that directory's
/// authority, so a file somebody else planted there is not the daemon's
/// document.
///
/// Pure over the two `stat` pairs so every rule is testable without needing a
/// second uid on the host. The deliberately-absent fourth rule is
/// `file_uid == geteuid()`, which the [`crate::workload`] ledger uses because
/// it writes its own files; here the producer is privileged and the reader is
/// not, and that check would refuse the document on every unprivileged read.
fn published_document_is_trusted(
    file_is_regular: bool,
    file_mode: u32,
    file_uid: u32,
    parent_mode: u32,
    parent_uid: u32,
) -> bool {
    file_is_regular && file_mode & 0o022 == 0 && parent_mode & 0o022 == 0 && file_uid == parent_uid
}

/// The published budget sample, parsed and trusted at `now_unix_ms`.
///
/// `None` on a missing document, a missing or `null` `gpu_budget` field (an
/// honest never-bound report), a malformed sample, or a sample that fails
/// [`GpuBudgetTelemetry::trusted_available_at`] — including a bound-but-stale
/// one. Callers that must tell never-bound from untrusted parse first with
/// [`GpuBudgetTelemetry::from_published_status`] and classify themselves.
pub(crate) fn trusted_budget(
    status: &serde_json::Value,
    now_unix_ms: Option<u64>,
) -> Option<GpuBudgetTelemetry> {
    let budget = GpuBudgetTelemetry::from_published_status(status)?;
    let now = now_unix_ms?;
    budget
        .trusted_available_at(now, ramshared_vram::GPU_BUDGET_MAX_AGE_MS)
        .map(|_| budget)
}

/// The published adapter busy-time sample, parsed and trusted at `now_unix_ms`.
///
/// `None` on a missing document, a missing or `null` `gpu_utilization` field
/// (an honest "not measured" report), a malformed sample, or a sample that
/// fails [`GpuUtilizationTelemetry::trusted_at`]. The freshness window is
/// [`ramshared_vram::GPU_BUDGET_MAX_AGE_MS`] because the busy-time sample is
/// taken at the same heartbeat as the budget — one adapter identity, one
/// timestamp — so they age out together.
pub(crate) fn trusted_utilization(
    status: &serde_json::Value,
    now_unix_ms: Option<u64>,
) -> Option<GpuUtilizationTelemetry> {
    let sample = GpuUtilizationTelemetry::from_published_status(status)?;
    let now = now_unix_ms?;
    sample
        .trusted_at(now, ramshared_vram::GPU_BUDGET_MAX_AGE_MS)
        .then_some(sample)
}

/// Reduce a published `gpu_cache` field value to a trusted sample.
///
/// This is **the** reduction of that field. The cascade calls it on the
/// daemon's document and the monitor calls it on the composed one; the
/// freshness gate is defined once so the two cannot drift into disagreeing
/// about what "live" means.
///
/// `None` on an absent or `null` field (an honest "no cache telemetry"), a
/// malformed sample, an unknown `schema_version`, or a sample outside
/// [`ramshared_vram::TELEMETRY_MAX_AGE_MS`]. A field the daemon did not
/// report is never a zero-filled sample.
///
/// This is the **admission** collapse and wants exactly "present but not
/// trustworthy": the alarm built on it has to fire for a malformed sample and
/// for an aged one alike. It is not the source of the published record —
/// that keeps an aged sample (see `cascade::cache_record`), the same split the
/// budget and the busy time already apply.
pub(crate) fn trusted_cache_field(
    field: Option<&serde_json::Value>,
    now_unix_ms: Option<u64>,
) -> Option<WorkerCacheTelemetry> {
    let cache = serde_json::from_value::<WorkerCacheTelemetry>(field?.clone()).ok()?;
    let now = now_unix_ms?;
    cache
        .trusted_at(now, ramshared_vram::TELEMETRY_MAX_AGE_MS)
        .then_some(cache)
}

/// The published cache-occupancy sample, parsed and trusted at `now_unix_ms`.
///
/// Document-level sibling of [`trusted_budget`] and [`trusted_utilization`]:
/// same shape, same honesty rule. `None` means "no trustworthy sample", never
/// "the cache is empty".
pub(crate) fn trusted_cache_telemetry(
    status: &serde_json::Value,
    now_unix_ms: Option<u64>,
) -> Option<WorkerCacheTelemetry> {
    trusted_cache_field(status.get("gpu_cache"), now_unix_ms)
}

/// Reduce a published `origin_cache_io` field value to its typed accounting.
///
/// Document-level counters, not a heartbeat sample: they are the daemon's own
/// in-process accounting, as fresh as the document that carries them, and that
/// document is already freshness- and identity-gated before anyone calls this.
/// There is therefore no second window here — a sample would need one.
///
/// `None` on an absent field (an older producer that never published it), a
/// `null` field, or a body that will not parse. Absence is "not measured",
/// never a zero-filled accounting. A present `0` is a real zero count.
pub(crate) fn trusted_origin_cache_io(field: Option<&serde_json::Value>) -> Option<OriginCacheIo> {
    let value = field?;
    if value.is_null() {
        return None;
    }
    serde_json::from_value::<OriginCacheIo>(value.clone()).ok()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use ramshared_vram::{
        GPU_BUDGET_MAX_AGE_MS, GpuAdapterIdentity, GpuBudgetSource, TELEMETRY_MAX_AGE_MS,
    };

    fn driver_status(sampled_at_unix_ms: u64) -> serde_json::Value {
        serde_json::json!({
            "ok": true,
            "gpu_budget": {
                "schema_version": 1,
                "adapter": { "backend": "cuda", "key": "0", "luid": null },
                "total_bytes": 8_000,
                "budget_bytes": 6_000,
                "used_bytes": 2_000,
                "available_bytes": 4_000,
                "source": "driver_reported",
                "sampled_at_unix_ms": sampled_at_unix_ms,
            }
        })
    }

    /// The path is declared once. A second declaration is how two readers
    /// start reading two different documents.
    #[test]
    fn cache_status_path_is_declared_once() {
        assert_eq!(CACHE_STATUS_FILE, "/run/ramshared/cache-status.json");
    }

    /// The reader is fail-closed and takes its path from the caller: an
    /// absent file, a non-JSON body and a well-formed body are three distinct
    /// outcomes, and none of them invents a document. The trust rules are a
    /// fourth refusal: a document a peer can rewrite is not the daemon's.
    #[test]
    fn read_status_document_is_fail_closed() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!(
            "rs-published-budget-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).expect("fixture directory");

        let missing = dir.join("missing.json");
        assert!(read_status_document(&missing).is_none());

        let malformed = dir.join("malformed.json");
        std::fs::write(&malformed, "not json").expect("write malformed body");
        assert!(read_status_document(&malformed).is_none());

        let well_formed = dir.join("well-formed.json");
        std::fs::write(&well_formed, driver_status(1_000).to_string()).expect("write body");
        let document = read_status_document(&well_formed).expect("well-formed body parses");
        assert!(GpuBudgetTelemetry::from_published_status(&document).is_some());

        // Group-writable and world-writable bodies are both substitutions
        // waiting to happen: the owner is not the only writer. The mode is
        // set after the write because the create-time mode is filtered
        // through the umask and would silently land back on 0644.
        let body = driver_status(1_000).to_string();
        for (name, mode) in [
            ("group-writable.json", 0o664),
            ("world-writable.json", 0o666),
        ] {
            let path = dir.join(name);
            std::fs::write(&path, &body).expect("write writable body");
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode))
                .expect("make body writable by a peer");
            assert!(
                read_status_document(&path).is_none(),
                "{name} must be refused: someone other than the owner can rewrite it"
            );
        }

        // A symlink is not a published document, even when its target is
        // perfectly well-formed: the path names a link a peer could repoint.
        let link = dir.join("linked.json");
        std::os::unix::fs::symlink(&well_formed, &link).expect("plant symlink");
        assert!(
            read_status_document(&link).is_none(),
            "a symlink must be refused even when its target parses"
        );

        // A directory is not a document.
        let nested = dir.join("nested");
        std::fs::create_dir_all(&nested).expect("nested directory");
        assert!(read_status_document(&nested).is_none());

        // A publication point a peer can write is not a publication point.
        let shared = dir.join("shared");
        std::fs::create_dir_all(&shared).expect("shared directory");
        std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o775))
            .expect("make directory group-writable");
        let planted = shared.join("planted.json");
        std::fs::write(&planted, &body).expect("write planted body");
        assert!(
            read_status_document(&planted).is_none(),
            "a document in a group-writable directory must be refused"
        );

        // A document planted in somebody else's directory is not the
        // publication point's document. The owner half of that rule is the
        // one a single-uid host cannot stage through the filesystem, so it
        // is pinned on the pure decision below; here the mode halves that
        // the filesystem *can* stage are pinned through the real read.

        std::fs::remove_dir_all(&dir).ok();
    }

    /// A fifo at the path is refused without hanging the open.
    ///
    /// The descriptor reader opens before it can `fstat`, so the open itself
    /// must not block on a non-regular file. `O_NONBLOCK` plus the type check
    /// is that refusal.
    #[test]
    fn read_status_document_refuses_a_fifo_without_hanging() {
        let dir =
            std::env::temp_dir().join(format!("rs-published-budget-fifo-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("fixture directory");
        let path = dir.join("cache-status.json");
        rustix::fs::mkfifoat(
            rustix::fs::CWD,
            &path,
            rustix::fs::Mode::from_bits_truncate(0o600),
        )
        .expect("plant fifo");
        assert!(
            read_status_document(&path).is_none(),
            "a fifo must be refused rather than hang the open"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The trust decision, rule by rule, without needing a second uid on the
    /// host. The `st_uid == geteuid()` shape the [`crate::workload`] ledger
    /// uses is deliberately absent and pinned here as absent: the producer is
    /// privileged and the reader is not.
    #[test]
    fn published_document_trust_is_owned_by_the_publication_point() {
        // The legitimate shape: a root-owned daemon document in a root-owned
        // directory, read by an unprivileged operator. The euid of this test
        // process is not root and must not matter.
        assert!(published_document_is_trusted(true, 0o644, 0, 0o755, 0));

        // A body the owner alone can rewrite is the rule the `workload.rs`
        // check expresses for its own files, and it applies here too — just
        // not the euid half.
        assert!(published_document_is_trusted(
            true, 0o644, 1000, 0o755, 1000
        ));

        // Not a regular file.
        assert!(!published_document_is_trusted(false, 0o644, 0, 0o755, 0));
        // Group- or world-writable document.
        assert!(!published_document_is_trusted(true, 0o664, 0, 0o755, 0));
        assert!(!published_document_is_trusted(true, 0o646, 0, 0o755, 0));
        // Group- or world-writable publication point.
        assert!(!published_document_is_trusted(true, 0o644, 0, 0o775, 0));
        assert!(!published_document_is_trusted(true, 0o644, 0, 0o757, 0));
        // The document's owner is not the publication point's authority — the
        // file a peer planted in somebody else's directory.
        assert!(!published_document_is_trusted(true, 0o644, 1000, 0o755, 0));
        assert!(!published_document_is_trusted(true, 0o644, 0, 0o755, 1000));
    }

    /// A fresh, driver-reported sample survives the trust path.
    #[test]
    fn trusted_budget_accepts_a_fresh_driver_sample() {
        let status = driver_status(1_000);
        let budget = trusted_budget(&status, Some(1_000 + 1)).expect("fresh sample is trusted");
        assert_eq!(budget.source, GpuBudgetSource::DriverReported);
        assert_eq!(
            budget.adapter,
            Some(GpuAdapterIdentity {
                node_mask: None,
                backend: "cuda".to_string(),
                key: "0".to_string(),
                luid: None,
            })
        );
    }

    /// Trust is the second gate: a bound sample that is too old is refused
    /// here, while the parse path still sees it — so a caller can classify it
    /// `stale` instead of `absent`.
    #[test]
    fn trusted_budget_refuses_an_untrusted_sample() {
        let status = driver_status(1_000);
        let aged = 1_000 + GPU_BUDGET_MAX_AGE_MS + 1;
        assert!(trusted_budget(&status, Some(aged)).is_none());
        assert!(GpuBudgetTelemetry::from_published_status(&status).is_some());

        let never_bound = serde_json::json!({ "gpu_budget": null });
        assert!(trusted_budget(&never_bound, Some(aged)).is_none());
    }

    fn utilization_status(sampled_at_unix_ms: u64) -> serde_json::Value {
        serde_json::json!({
            "ok": true,
            "gpu_utilization": {
                "schema_version": 1,
                "gpu_pct": 37,
                "memory_pct": 82,
                "sampled_at_unix_ms": sampled_at_unix_ms,
            }
        })
    }

    /// A measured idle sample is a real sample: `0` percents survive the trust
    /// path and are never confused with the absence of a sample.
    #[test]
    fn trusted_utilization_carries_a_measured_idle_and_refuses_absence() {
        let idle = serde_json::json!({
            "gpu_utilization": {
                "schema_version": 1,
                "gpu_pct": 0,
                "memory_pct": 0,
                "sampled_at_unix_ms": 1_000,
            }
        });
        let sample = trusted_utilization(&idle, Some(1_001)).expect("measured idle is trusted");
        assert_eq!(sample.gpu_pct, 0);
        assert_eq!(sample.memory_pct, 0);

        let not_measured = serde_json::json!({ "gpu_utilization": null });
        assert!(trusted_utilization(&not_measured, Some(1_001)).is_none());
        let missing_key = serde_json::json!({ "ok": true });
        assert!(trusted_utilization(&missing_key, Some(1_001)).is_none());
    }

    /// Trust is the second gate here too: a bound sample that is too old is
    /// refused, and an out-of-range percent is refused rather than clamped.
    #[test]
    fn trusted_utilization_refuses_stale_and_out_of_range_samples() {
        let status = utilization_status(1_000);
        let aged = 1_000 + GPU_BUDGET_MAX_AGE_MS + 1;
        assert!(trusted_utilization(&status, Some(aged)).is_none());
        assert!(GpuUtilizationTelemetry::from_published_status(&status).is_some());

        let out_of_range = serde_json::json!({
            "gpu_utilization": {
                "schema_version": 1,
                "gpu_pct": 101,
                "memory_pct": 82,
                "sampled_at_unix_ms": 1_000,
            }
        });
        assert!(trusted_utilization(&out_of_range, Some(1_000)).is_none());

        let no_clock = utilization_status(1_000);
        assert!(trusted_utilization(&no_clock, None).is_none());
    }

    fn cache_status(logical_cached_bytes: u64, sampled_at_unix_ms: u64) -> serde_json::Value {
        serde_json::json!({
            "ok": true,
            "gpu_cache": {
                "schema_version": 1,
                "sampled_at_unix_ms": sampled_at_unix_ms,
                "codec": {
                    "capability": "available",
                    "state": "ready",
                    "refusal_reason": null
                },
                "logical_cached_bytes": logical_cached_bytes,
                "physical_cache_slab_bytes": 8 * 1024 * 1024,
                "codec_workspace_bytes": 1024 * 1024,
                "compressed_payload_bytes": 32 * 1024 * 1024,
                "raw_payload_bytes": 4 * 1024 * 1024,
                "metadata_bytes": 64 * 1024,
                "raw_bypass_bytes": 16 * 1024 * 1024,
                "codec_integrity_errors": 0,
                "codec_decode_errors": 1,
                "codec_timeouts": 0
            }
        })
    }

    /// The reduction is defined once and it is the one the monitor uses, so
    /// the cascade and the dashboard cannot drift into disagreeing about what
    /// "live" means.
    #[test]
    fn trusted_cache_field_is_the_single_reduction_of_the_cache_sample() {
        let status = cache_status(256 * 1024 * 1024, 1_000);
        let sample =
            trusted_cache_telemetry(&status, Some(1_001)).expect("fresh sample is trusted");
        assert_eq!(sample.logical_cached_bytes, 256 * 1024 * 1024);
        assert_eq!(sample.schema_version, 1);

        // The field-level entry point is the same reduction.
        let field = status.get("gpu_cache");
        assert_eq!(
            trusted_cache_field(field, Some(1_001)).map(|c| c.logical_cached_bytes),
            Some(256 * 1024 * 1024)
        );
    }

    /// Absence is absence: a missing or `null` `gpu_cache` is "no cache
    /// telemetry", never a zero-filled sample that would read as an empty
    /// cache the daemon actually measured.
    #[test]
    fn trusted_cache_absence_is_not_an_empty_cache() {
        let missing_key = serde_json::json!({ "ok": true });
        assert!(trusted_cache_telemetry(&missing_key, Some(1_000)).is_none());

        let null_field = serde_json::json!({ "gpu_cache": null });
        assert!(trusted_cache_telemetry(&null_field, Some(1_000)).is_none());

        assert!(trusted_cache_field(None, Some(1_000)).is_none());
    }

    /// Trust is the second gate: stale, unknown-schema and malformed samples
    /// are refused, never published as if they were live.
    #[test]
    fn trusted_cache_refuses_stale_unknown_schema_and_malformed() {
        let status = cache_status(256 * 1024 * 1024, 1_000);
        let aged = 1_000 + TELEMETRY_MAX_AGE_MS + 1;
        assert!(trusted_cache_telemetry(&status, Some(aged)).is_none());
        // The parse path still sees it, so a caller can classify `stale`
        // instead of `absent`.
        assert!(trusted_cache_field(status.get("gpu_cache"), Some(1_000)).is_some());

        let mut unknown_version = status.clone();
        unknown_version["gpu_cache"]["schema_version"] = serde_json::json!(99);
        assert!(trusted_cache_telemetry(&unknown_version, Some(1_000)).is_none());

        let malformed = serde_json::json!({ "gpu_cache": { "logical_cached_bytes": 16 } });
        assert!(trusted_cache_telemetry(&malformed, Some(1_000)).is_none());

        let no_clock = cache_status(256 * 1024 * 1024, 1_000);
        assert!(trusted_cache_telemetry(&no_clock, None).is_none());
    }

    /// A present accounting is taken as measured, counts and gauge alike, and
    /// a real zero stays a real zero.
    #[test]
    fn origin_cache_io_reduction_takes_a_measured_accounting_as_is() {
        let status = serde_json::json!({
            "origin_cache_io": {
                "origin_syncs": 3,
                "batched_writes": 5,
                "cache_read_bytes": 8_192,
                "cache_read_failures": 0,
                "cache_write_failures": 1,
                "promotion_refusals": 0,
                "allocation_failures": 2,
                "valid_blocks": 0
            }
        });
        let published = trusted_origin_cache_io(status.get("origin_cache_io"))
            .expect("a present, well-formed accounting is measured");
        assert_eq!(published.origin_syncs, 3);
        assert_eq!(published.batched_writes, 5);
        assert_eq!(published.cache_read_bytes, 8_192);
        assert_eq!(published.cache_read_failures, 0);
        assert_eq!(published.cache_write_failures, 1);
        assert_eq!(published.promotion_refusals, Some(0));
        assert_eq!(published.allocation_failures, Some(2));
        // A gauge at zero is a measured empty cache, not an absent sample.
        assert_eq!(published.valid_blocks, Some(0));
    }

    #[test]
    fn isolated_cache_unknown_counters_survive_status_parsing() {
        let status = serde_json::json!({ "origin_cache_io": {
            "origin_syncs": 1, "batched_writes": 2, "cache_read_bytes": 3,
            "cache_read_failures": 0, "cache_write_failures": 0,
            "promotion_refusals": null, "allocation_failures": null, "valid_blocks": null
        }});
        let io = trusted_origin_cache_io(status.get("origin_cache_io")).unwrap();
        assert_eq!(io.batched_writes, 2);
        assert_eq!(io.promotion_refusals, None);
        assert_eq!(io.allocation_failures, None);
        assert_eq!(io.valid_blocks, None);
    }

    /// Absence is "not measured" and is never collapsed into a zero-filled
    /// accounting. An older producer, an honest `null` and a body that will not
    /// parse all publish absence.
    #[test]
    fn origin_cache_io_absence_is_not_a_zero_filled_accounting() {
        let absent = serde_json::json!({ "cache_target_kib": 1 });
        assert!(trusted_origin_cache_io(absent.get("origin_cache_io")).is_none());
        assert!(trusted_origin_cache_io(None).is_none());

        let null_field = serde_json::json!({ "origin_cache_io": null });
        assert!(trusted_origin_cache_io(null_field.get("origin_cache_io")).is_none());

        let malformed = serde_json::json!({ "origin_cache_io": { "origin_syncs": "many" } });
        assert!(trusted_origin_cache_io(malformed.get("origin_cache_io")).is_none());
    }
}

#[cfg(test)]
mod bounds_tests;
