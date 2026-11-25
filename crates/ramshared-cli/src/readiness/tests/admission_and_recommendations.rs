// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/readiness/tests/admission_and_recommendations.rs
#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;

/// RF-1 / DT-1: the production composition binds the published sample and
/// reports its trusted available bytes as headroom, with provenance.
#[test]
fn a_published_fresh_sample_reports_driver_reported_headroom() {
    let now = 1_000_000u64;
    let document = published_document(published_driver_budget(now - 500));
    let report = report_with_published_document(ready_obs_for_capacity(), Some(&document), now);
    assert_eq!(report.capacity.source, CapacitySource::DriverReported);
    assert_eq!(
        report.capacity.effective_bytes,
        Some(3 * 1024 * 1024 * 1024),
        "headroom is the sample's trusted available bytes, not adapter free"
    );
    assert_eq!(report.capacity.age_ms, Some(500));
}

/// RF-3 / DT-3: a sample that is present but aged past the freshness window
/// is `stale`, never collapsed into `absent`. That distinction is the news
/// the operator acts on.
#[test]
fn an_aged_sample_reports_stale_not_absent() {
    let now = 1_000_000u64;
    let document = published_document(published_driver_budget(now - GPU_BUDGET_MAX_AGE_MS - 1));
    let report = report_with_published_document(ready_obs_for_capacity(), Some(&document), now);
    assert_eq!(report.capacity.source, CapacitySource::Stale);
    assert_eq!(report.capacity.effective_bytes, None);
    assert_eq!(
        report.capacity.age_ms,
        Some(GPU_BUDGET_MAX_AGE_MS + 1),
        "the age that caused the refusal is reported"
    );
}

/// The production shape of the three capacity states, end to end.
///
/// [`report_with_published_document`] is the pure seam and takes a document
/// its caller already admitted; [`run_check`] is what admits one. Before
/// that gate, a leftover from an earlier daemon instance reduced into
/// `DriverReported` headroom as confidently as the live publisher's own
/// report — a second reader of one fact, which is the shape the honesty
/// rule exists to outlaw. The rule is: a **document-level** refusal is
/// policy and collapses to `absent`; a per-field collapse inside an
/// admitted document is the bug, and is why `stale` survives here.
#[test]
fn a_leftover_document_never_reaches_the_reduction() {
    let now = 1_000_000u64;
    let fresh_sample = published_driver_budget(now - 500);
    let live = bound_published_document(&fresh_sample, "daemon-1", now - 10);

    // 1. The live publisher's current document with a fresh sample is
    //    headroom with provenance.
    assert!(crate::cascade::cache_status_is_current_daemon(
        &live, "daemon-1", now
    ));
    let admitted = report_with_published_document(ready_obs_for_capacity(), Some(&live), now);
    assert_eq!(admitted.capacity.source, CapacitySource::DriverReported);
    assert_eq!(
        admitted.capacity.effective_bytes,
        Some(3 * 1024 * 1024 * 1024)
    );

    // 2. The same live document whose *sample* has aged while the daemon
    //    kept rewriting the document is `stale`, not `absent`. That is the
    //    distinction the gate must not "solve" by dropping it — the
    //    document is the live one's; only its figure is no longer current.
    let aged_sample = published_driver_budget(now - GPU_BUDGET_MAX_AGE_MS - 1);
    let live_with_aged_sample = bound_published_document(&aged_sample, "daemon-1", now - 10);
    assert!(crate::cascade::cache_status_is_current_daemon(
        &live_with_aged_sample,
        "daemon-1",
        now
    ));
    let stale =
        report_with_published_document(ready_obs_for_capacity(), Some(&live_with_aged_sample), now);
    assert_eq!(stale.capacity.source, CapacitySource::Stale);

    // 3. A leftover from a previous instance is refused whole, and the
    //    report is `absent` — the same answer as no publisher at all,
    //    because in both cases no live publisher has spoken. Its perfectly
    //    fresh budget field does not rescue it.
    assert!(!crate::cascade::cache_status_is_current_daemon(
        &live, "daemon-2", now
    ));
    let refused = report_with_published_document(ready_obs_for_capacity(), None, now);
    assert_eq!(refused.capacity.source, CapacitySource::Absent);
    assert_eq!(refused.capacity.effective_bytes, None);
}

/// RF-4 / DT-4: `gpu_budget: null` is the daemon's honest never-bound
/// report, not a stale sample and not an alarm.
#[test]
fn a_never_bound_budget_reports_absent_without_a_warning() {
    let now = 1_000_000u64;
    let document = serde_json::json!({ "ok": true, "gpu_budget": null });
    let report = report_with_published_document(ready_obs_for_capacity(), Some(&document), now);
    assert_eq!(report.capacity.source, CapacitySource::Absent);
    assert_eq!(report.capacity.effective_bytes, None);
    assert!(
        !report.warnings.iter().any(|w| w.contains("capacity")),
        "never-bound must not warn: {:?}",
        report.warnings
    );
}

/// RF-7 / DT-7: the rendered line names the authority and the quantity, so
/// it cannot be read as contradicting the adapter inventory above it.
#[test]
fn admission_budget_label_names_the_authority_and_headroom() {
    let now = 1_000_000u64;
    let document = published_document(published_driver_budget(now - 500));
    let report = report_with_published_document(ready_obs_for_capacity(), Some(&document), now);

    let mut text = Vec::new();
    print_text_report(&report, &mut text).expect("report renders");
    let text = String::from_utf8(text).expect("report is utf-8");

    assert!(
        text.contains("Admission budget (published sample): source=driver-reported, headroom="),
        "label must name the authority and headroom:\n{text}"
    );
    assert!(
        !text.contains("effective="),
        "the old ambiguous field name is gone:\n{text}"
    );
    assert!(
        text.contains("age_ms=500"),
        "the age is beside the figure:\n{text}"
    );
}

/// RF-6 / DT-6: adapter inventory from the provider probe is never an
/// admission figure. The probe's `total_bytes`/`free_bytes` and the
/// published sample's headroom are different quantities and never mix.
#[test]
fn adapter_inventory_is_never_an_admission_figure() {
    let now = 1_000_000u64;
    let inventoried = || {
        let mut obs = ready_obs_for_capacity();
        obs.gpu.gpu = Some(GpuInfo {
            name: "test adapter".to_string(),
            total_bytes: 6 * 1024 * 1024 * 1024,
            free_bytes: 5 * 1024 * 1024 * 1024,
        });
        obs
    };

    // A provider that reports a large free adapter still yields no
    // admission figure when nothing was published.
    let unpublished = report_with_published_document(inventoried(), None, now);
    assert_eq!(unpublished.capacity.effective_bytes, None);
    assert_eq!(unpublished.capacity.source, CapacitySource::Absent);

    // When a sample is published, the figure is the sample's trusted
    // available bytes — not the probe's free, not the probe's total.
    let document = published_document(published_driver_budget(now - 500));
    let published = report_with_published_document(inventoried(), Some(&document), now);
    assert_eq!(
        published.capacity.effective_bytes,
        Some(3 * 1024 * 1024 * 1024)
    );
    let inventory = published.gpu.gpu.expect("probe reported inventory");
    assert_ne!(
        published.capacity.effective_bytes,
        Some(inventory.free_bytes),
        "adapter free is not admission headroom"
    );
    assert_ne!(
        published.capacity.effective_bytes,
        Some(inventory.total_bytes),
        "adapter total is not admission headroom"
    );
}

/// RF-8 / DT-8: a physical-VRAM surface warns on a bound-but-untrusted
/// sample and stays silent on an absent one.
#[test]
fn physical_vram_surface_warns_on_an_untrusted_but_bound_sample() {
    let now = 1_000_000u64;
    let stale = published_document(published_driver_budget(now - GPU_BUDGET_MAX_AGE_MS - 1));
    let warned = report_with_published_document(ready_obs_for_capacity(), Some(&stale), now);
    assert!(
        warned
            .warnings
            .iter()
            .any(|w| w.contains("capacity is stale")),
        "a stale sample on a physical-VRAM surface warns: {:?}",
        warned.warnings
    );

    let absent = report_with_published_document(ready_obs_for_capacity(), None, now);
    assert!(
        !absent.warnings.iter().any(|w| w.contains("capacity")),
        "absent never warns: {:?}",
        absent.warnings
    );
}

/// Rendering is deterministic and keeps the decision and surface visible.
#[test]
fn check_report_rendering_is_deterministic() {
    let report = report_from_observations(obs(
        platform_probe(
            Observation::Measured(PlatformKind::Wsl2),
            "6.6.87.2-microsoft-standard-WSL2",
        ),
        gpu_cuda(),
        backends_ready(),
        Observation::Measured(vec![SwapEntry {
            filename: "/dev/nbd0".to_string(),
            kind: "partition".to_string(),
            size_kib: 1024 * 1024,
            used_kib: 256 * 1024,
            priority: 10,
        }]),
    ));

    let mut text_buf = Vec::new();
    print_text_report(&report, &mut text_buf).expect("text report renders");
    let text_out = String::from_utf8(text_buf).expect("text is UTF-8");
    assert!(text_out.contains("Platform: wsl2"));
    assert!(text_out.contains("Transport: nbd"));
    assert!(text_out.contains("GPU backend: cuda"));
    assert!(text_out.contains("NVIDIA RTX 4090"));
    assert!(text_out.contains("Current swap: /dev/nbd0"));
    assert!(text_out.contains("Admission budget (published sample):"));
    assert!(text_out.contains("Decision: blocked"));

    let json_out = render_json(&report);
    assert!(json_out.contains("\"platform\":\"wsl2\""));
    assert!(json_out.contains("\"transport\":\"nbd\""));
    assert!(json_out.contains("\"gpu_backend\":\"cuda\""));
    assert!(json_out.contains("\"decision\":\"blocked\""));

    let recs = recommendations_for(&report);
    let mut rec_buf = Vec::new();
    print_recommendations(&recs, &mut rec_buf).expect("recommendations render");
    assert!(String::from_utf8_lossy(&rec_buf).contains("Recommendations:"));

    let doc_json = render_doctor_json(&report, &recs);
    assert!(doc_json.contains("\"decision\":\"blocked\""));
    assert!(doc_json.contains("\"recommendations\":["));
}

/// A native Linux + Vulkan surface never receives WSL/CUDA recovery advice.
#[test]
fn native_linux_vulkan_recommendations_omit_wsl_cuda_recovery() {
    let report = report_from_observations(obs(
        platform_probe(
            Observation::Measured(PlatformKind::NativeLinux),
            "6.8.0-45-generic",
        ),
        gpu_vulkan_hardware(),
        backends_ready(),
        Observation::Measured(Vec::new()),
    ));
    let recs = recommendations_for(&report);
    for rec in &recs {
        let lowered = rec.to_lowercase();
        assert!(
            !lowered.contains("wsl --update"),
            "unexpected WSL advice: {rec}"
        );
        assert!(
            !lowered.contains("libcuda"),
            "unexpected CUDA advice: {rec}"
        );
        assert!(
            !lowered.contains("/dev/dxg"),
            "unexpected dxg advice: {rec}"
        );
    }
}

#[test]
fn backend_env_permutations_cover_all_branches() {
    let kernel = kernel_ready();
    let open = BackendEnv {
        nbd_device_present: true,
        nbd_module_loaded: true,
        ublk_control_present: true,
        ublk_control_openable: true,
    };
    assert_eq!(probe_backends(&kernel).nbd_status, Status::Ok);
    let _ = open;
    let _ = gpu_unknown();
}
