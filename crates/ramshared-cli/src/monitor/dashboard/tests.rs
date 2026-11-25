// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/monitor/dashboard/tests.rs
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
use super::*;
use crate::monitor::model::{
    CgroupMemoryObservation, ControlPlaneObservation, HyperVBalloonObservation, MemoryObservation,
    MemoryScope, ProcessMemoryTotals, TierDeviceNames,
};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use serde_json::{Map, json};

/// The pressure panel names the authority's own state and the lease
/// deadline, so the operator can tell "the peer has not arrived" from
/// "the lease is live" without reading the JSON.
#[test]
fn the_pressure_panel_names_the_control_plane_state() {
    let mut observation = active_cache_observation();
    observation
        .status
        .insert("control_plane_state".to_string(), json!("vsock_leased"));
    observation.status.insert(
        "control_plane_lease_deadline_ms".to_string(),
        json!(20_000u64),
    );
    let line = control_plane_line(&observation, Language::PtBr);
    assert!(line.contains("PLANO DE CONTROLE"), "{line}");
    assert!(line.contains("vsock_leased"), "{line}");
    assert!(line.contains("19 s"), "{line}");
    let english = control_plane_line(&observation, Language::En);
    assert!(english.contains("CONTROL PLANE"), "{english}");
}

/// The footer's "now" line reads the origin's state in the console's
/// language. A hardcoded Portuguese adjective inside the English sentence
/// ("Origin PRONTA") was the visible symptom.
#[test]
fn the_footer_names_the_origin_state_in_the_console_language() {
    let mut observation = active_cache_observation();
    observation
        .status
        .insert("origin_state".to_string(), json!("READY"));
    let english = action_now_line(&observation, Language::En);
    assert!(english.contains("Origin READY"), "{english}");
    assert!(!english.contains("PRONTA"), "{english}");
    let portuguese = action_now_line(&observation, Language::PtBr);
    assert!(portuguese.contains("PRONTO"), "{portuguese}");
    assert!(!portuguese.contains("PRONTA"), "{portuguese}");
}

/// An empty cache must render, not panic.
///
/// The hit rate divides by the total of the hits and the origin reads. The
/// guard was written as `then_some`, which evaluates its argument
/// eagerly, so the division ran before the guard and the dashboard
/// panicked with "attempt to divide by zero" the moment the cache was
/// empty — which is exactly the state a fresh or drained cache has.
#[test]
fn an_empty_cache_renders_without_panicking() {
    let mut observation = active_cache_observation();
    observation
        .status
        .insert("cache_hits".to_string(), json!(0u64));
    observation
        .status
        .insert("cache_fallback_reads".to_string(), json!(0u64));
    let snapshot = cache_snapshot(&observation);
    assert_eq!(snapshot.hit_rate(), None, "no traffic means no rate");
    let mut terminal = Terminal::new(TestBackend::new(120, 40)).expect("terminal");
    terminal
        .draw(|frame| {
            draw(
                frame,
                &observation,
                &DashHistory::default(),
                &metadata(),
                Language::En,
                Screen::Main,
            )
        })
        .expect("the dashboard must render an empty cache");
}

/// A guarded control plane is not by itself memory pressure.
///
/// The live post-reboot console showed `Overall: DEGRADED — control
/// reserve` while `MemAvailable` was ~11 GiB against a 4 GiB reserve. The
/// cause fell through to `control reserve` whenever no measured threshold
/// matched, so a fail-closed GUARDED verdict (no daemon, or the
/// supervisor's recovery window) was reported as a shortage it never
/// measured.
#[test]
fn a_guarded_console_without_pressure_names_no_reserve_cause() {
    let mut observation = active_cache_observation();
    observation.mem.available_kib = Some(11 * 1024 * 1024);
    assert_eq!(observation.string("overall_state"), "GUARDED");
    assert_eq!(degraded_cause(&observation, Language::En), None);

    let mut terminal = Terminal::new(TestBackend::new(120, 40)).expect("terminal");
    terminal
        .draw(|frame| {
            draw(
                frame,
                &observation,
                &DashHistory::default(),
                &metadata(),
                Language::En,
                Screen::Main,
            )
        })
        .expect("draw");
    let buffer = terminal.backend().buffer();
    let width = buffer.area().width as usize;
    let rendered = buffer
        .content()
        .chunks(width)
        .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n");
    let header = rendered
        .lines()
        .find(|line| line.contains("Overall:"))
        .expect("the header renders the overall verdict");
    assert!(header.contains("DEGRADED"), "{header}");
    assert!(
        !header.contains("control reserve"),
        "a guarded verdict with 11 GiB available must not claim reserve pressure: {header}"
    );
}

#[test]
fn a_guarded_console_below_the_reserve_names_the_reserve() {
    let mut observation = active_cache_observation();
    observation.mem.available_kib = Some(3 * 1024 * 1024);
    let cause = degraded_cause(&observation, Language::En).expect("reserve cause");
    assert!(cause.contains("reserve"), "{cause}");
    assert!(cause.contains("3.00/4.00 GiB"), "{cause}");
}

#[test]
fn a_guarded_console_uses_the_supervisor_byte_precise_reserve() {
    let mut observation = active_cache_observation();
    observation.mem.total_kib = Some(16 * 1024 * 1024 + 1);
    observation.mem.available_kib = Some(4 * 1024 * 1024);
    let cause = degraded_cause(&observation, Language::En).expect("rounded reserve shortage");
    assert!(cause.contains("reserve"), "{cause}");
}

#[test]
fn a_guarded_console_in_the_recovery_band_names_the_recovery() {
    let mut observation = active_cache_observation();
    observation.mem.available_kib = Some(4 * 1024 * 1024 + 512 * 1024);
    let cause = degraded_cause(&observation, Language::En).expect("recovery cause");
    assert!(cause.contains("recovery"), "{cause}");
    assert!(cause.contains("4.50/5.00 GiB"), "{cause}");
}

#[test]
fn a_guarded_console_under_psi_names_the_pressure() {
    let mut observation = active_cache_observation();
    observation.mem.available_kib = Some(11 * 1024 * 1024);
    observation.control_plane.memory_psi_full_avg10 = 3.0;
    assert_eq!(
        degraded_cause(&observation, Language::En).as_deref(),
        Some("PSI full 3.00%")
    );
}

#[test]
fn guarded_cause_uses_psi_only_when_it_is_measured() {
    let mut observation = active_cache_observation();
    observation.mem.available_kib = Some(11 * 1024 * 1024);
    observation.control_plane.memory_psi_full_avg10 = 3.0;
    observation.control_plane.memory_psi_available = false;
    assert_eq!(degraded_cause(&observation, Language::En), None);
    observation.control_plane.memory_psi_available = true;
    observation.mem.total_kib = None;
    assert_eq!(
        degraded_cause(&observation, Language::En).as_deref(),
        Some("PSI full 3.00%")
    );
}

#[test]
fn a_healthy_console_never_names_a_pressure_cause() {
    let mut observation = active_cache_observation();
    observation
        .status
        .insert("overall_state".to_string(), json!("HEALTHY"));
    observation.mem.available_kib = Some(1024 * 1024);
    observation.control_plane.memory_psi_full_avg10 = 8.0;
    assert_eq!(degraded_cause(&observation, Language::En), None);
}

fn active_cache_observation() -> Observation {
    let status: Map<String, serde_json::Value> = serde_json::from_value(json!({
        "cache_state": "ACTIVE",
        "capacity": {
            "cache_target_kib": 33792u64,
            "logical_capacity_kib": 4194304u64,
            "vram_cached_kib": 0u64,
            "gpu_headroom_kib": 2786304u64
        },
        "cache_hits": 0u64,
        "cache_fallback_reads": 48u64,
        "origin_cache_io": {"cache_read_bytes": 0u64},
        "gpu_cache": {
            "schema_version": 1u8,
            "sampled_at_unix_ms": 1u64,
            "codec": {"capability": "raw_only", "state": "raw_only", "refusal_reason": "no-provider-codec"},
            "logical_cached_bytes": 466944u64,
            "physical_cache_slab_bytes": 0u64,
            "codec_workspace_bytes": 0u64,
            "compressed_payload_bytes": 0u64,
            "raw_payload_bytes": 0u64,
            "metadata_bytes": 0u64,
            "raw_bypass_bytes": 466944u64,
            "codec_integrity_errors": 0u64,
            "codec_decode_errors": 0u64,
            "codec_timeouts": 0u64
        },
        "tiers": {
            "zram": {"used_kib": 921600u64, "size_kib": 2097152u64},
            "disk": {"used_kib": 6600000u64, "size_kib": 8388608u64}
        },
        "protection_state": "READY",
        "origin_state": "READY",
        "overall_state": "GUARDED"
    })).expect("status fixture");
    Observation {
        status: status.into_iter().collect(),
        epoch_ms: 1,
        sample_age_ms: 0,
        memory_scope: MemoryScope::Wsl2,
        mem: MemoryObservation {
            total_kib: Some(16 * 1024 * 1024),
            available_kib: Some(5 * 1024 * 1024),
            swap_total_kib: Some(14 * 1024 * 1024),
            swap_free_kib: Some(6 * 1024 * 1024),
            ..MemoryObservation::default()
        },
        cgroup_memory: CgroupMemoryObservation::default(),
        hyperv_balloon: HyperVBalloonObservation::default(),
        process_totals: ProcessMemoryTotals::default(),
        control_plane: ControlPlaneObservation {
            memory_psi_available: true,
            memory_psi_full_avg10: 0.07,
            memory_psi_some_avg10: 0.07,
            memory_psi_full_avg60: 0.04,
            memory_psi_full_avg300: 0.0,
            ..ControlPlaneObservation::default()
        },
        gpu: None,
        tier_devices: TierDeviceNames::default(),
        operation_progress: None,
        top_processes: Vec::new(),
        errors: Vec::new(),
    }
}

/// The cache capacity lives under the status document's `capacity`
/// object. The dashboard must render those measured figures, not the
/// legacy top-level names the observation does not carry.
#[test]
fn capacity_panel_reads_the_published_capacity_object() {
    let observation = active_cache_observation();
    let cache = cache_snapshot(&observation);
    assert_eq!(cache.target_bytes, Some(33_792 * KIB));
    assert_eq!(cache.logical_capacity_bytes, Some(4 * GIB));
    assert_eq!(cache.headroom_bytes, Some(2_786_304 * KIB));
    assert_eq!(cache.fill_pct(), Some(1));

    let mut history = DashHistory::default();
    history.push_observation(&observation);
    assert_eq!(history.cache_fill.back(), Some(&1));

    let backend = TestBackend::new(120, 36);
    let mut terminal = Terminal::new(backend).expect("terminal");
    terminal
        .draw(|frame| {
            draw(
                frame,
                &observation,
                &history,
                &metadata(),
                Language::En,
                Screen::Main,
            )
        })
        .expect("draw");
    let buffer = terminal.backend().buffer();
    let width = buffer.area().width as usize;
    let rendered = buffer
        .content()
        .chunks(width)
        .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(rendered.contains("33.0 MiB"), "{rendered}");
    assert!(rendered.contains("4.0 GiB"), "{rendered}");
    assert!(rendered.contains("2.7 GiB"), "{rendered}");
}

/// A present `null` is an unmeasured capacity member. It must not fall
/// through to a legacy top-level duplicate and resurrect a stale figure.
/// Only an absent key uses the compatibility fallback.
#[test]
fn a_present_null_capacity_member_does_not_use_the_legacy_duplicate() {
    let mut observation = active_cache_observation();
    observation
        .status
        .get_mut("capacity")
        .and_then(|capacity| capacity.as_object_mut())
        .expect("capacity object")
        .insert("cache_target_kib".to_string(), json!(null));
    observation
        .status
        .insert("cache_target_kib".to_string(), json!(33_792u64));
    assert_eq!(cache_snapshot(&observation).target_bytes, None);

    observation.status.remove("capacity");
    assert_eq!(
        cache_snapshot(&observation).target_bytes,
        Some(33_792 * KIB),
        "an absent capacity object keeps the legacy shape readable"
    );
}

fn metadata() -> DashboardMetadata {
    DashboardMetadata {
        version_line: "RamShared test".to_string(),
        running_line: String::new(),
        install_lines: Vec::new(),
        restart_executable: None,
        runtime_notice: None,
        update_notice: None,
    }
}

#[test]
fn active_cache_renders_the_effectiveness_panel_without_fake_compression() {
    let observation = active_cache_observation();
    let mut history = DashHistory::default();
    history.push_observation(&observation);
    let backend = TestBackend::new(120, 36);
    let mut terminal = Terminal::new(backend).expect("terminal");
    terminal
        .draw(|frame| {
            draw(
                frame,
                &observation,
                &history,
                &metadata(),
                Language::PtBr,
                Screen::Main,
            )
        })
        .expect("draw");
    let buffer = terminal.backend().buffer();
    let width = buffer.area().width as usize;
    let rendered = buffer
        .content()
        .chunks(width)
        .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(rendered.contains("CACHE DA ORIGEM"), "{rendered}");
    assert!(rendered.contains("ATIVO"), "{rendered}");
    assert!(rendered.contains("na origem"), "{rendered}");
    assert!(rendered.contains("RawOnly"), "{rendered}");
    assert!(rendered.contains("[l] English"), "{rendered}");
    assert!(
        !rendered.contains("ratio"),
        "RawOnly must not invent a ratio: {rendered}"
    );
}

#[test]
fn pressure_shows_published_host_memory_without_inventing_psi() {
    let mut observation = active_cache_observation();
    observation.status.insert(
        "host_memory".to_string(),
        json!({
            "sampled_at_unix_ms": observation.epoch_ms,
            "total_bytes": 16u64 * GIB,
            "available_bytes": 4u64 * GIB,
            "commit_total_bytes": 8u64 * GIB,
            "commit_limit_bytes": 20u64 * GIB,
            "page_size_bytes": 4096u64
        }),
    );
    let backend = TestBackend::new(120, 36);
    let mut terminal = Terminal::new(backend).expect("terminal");
    terminal
        .draw(|frame| {
            draw(
                frame,
                &observation,
                &DashHistory::default(),
                &metadata(),
                Language::PtBr,
                Screen::Pressure,
            )
        })
        .expect("draw");
    let buffer = terminal.backend().buffer();
    let width = buffer.area().width as usize;
    let rendered = buffer
        .content()
        .chunks(width)
        .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(rendered.contains("WINDOWS (host)"), "{rendered}");
    assert!(rendered.contains("4,0 GiB"), "{rendered}");
    assert!(rendered.contains("12,0 GiB"), "{rendered}");
}

#[test]
fn main_dashboard_distinguishes_guest_and_fresh_host_available_memory() {
    let mut observation = active_cache_observation();
    observation.epoch_ms = 50_000;
    observation.status.insert(
        "host_memory".to_string(),
        json!({
            "sampled_at_unix_ms": 50_000u64,
            "total_bytes": 32u64 * GIB,
            "available_bytes": 768u64 * MIB,
            "commit_total_bytes": 8u64 * GIB,
            "commit_limit_bytes": 40u64 * GIB,
            "page_size_bytes": 4096u64
        }),
    );
    observation.status.insert(
        "zram_compression".to_string(),
        json!({
            "orig_data_bytes": 2u64 * GIB,
            "compr_data_bytes": 1100u64 * MIB,
            "mem_used_bytes": 1120u64 * MIB,
            "huge_pages": 24500u64,
            "compressor": "lzo-rle"
        }),
    );
    for (width, height) in [(120, 36), (80, 24)] {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| {
                draw(
                    frame,
                    &observation,
                    &DashHistory::default(),
                    &metadata(),
                    Language::PtBr,
                    Screen::Main,
                )
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        let rendered = buffer
            .content()
            .chunks(usize::from(width))
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("guest WSL2"), "{rendered}");
        assert!(rendered.contains("WINDOWS (host)"), "{rendered}");
        assert!(rendered.contains("768,0 MiB"), "{rendered}");
        assert!(rendered.contains("Pressão guest"), "{rendered}");
        assert!(rendered.contains("COMPRESSÃO"), "{rendered}");
        assert!(rendered.contains("1,86:1"), "{rendered}");
        if width == 80 {
            assert!(!rendered.contains(text(Language::PtBr).cost), "{rendered}");
        }
    }
}

#[test]
fn main_host_memory_line_refuses_stale_future_and_invalid_samples() {
    let mut observation = active_cache_observation();
    observation.epoch_ms = 50_000;
    let sample = |sampled_at, available| {
        json!({
            "sampled_at_unix_ms": sampled_at,
            "total_bytes": 32u64 * GIB,
            "available_bytes": available,
            "commit_total_bytes": 8u64 * GIB,
            "commit_limit_bytes": 40u64 * GIB,
            "page_size_bytes": 4096u64
        })
    };
    observation
        .status
        .insert("host_memory".into(), sample(34_999u64, 768u64 * MIB));
    let stale = host_memory_line(&observation, Language::PtBr);
    assert!(stale.contains("desatualizado"));
    assert!(!stale.contains("768"));
    observation
        .status
        .insert("host_memory".into(), sample(50_001u64, 768u64 * MIB));
    assert!(host_memory_line(&observation, Language::PtBr).contains("desatualizado"));
    observation
        .status
        .insert("host_memory".into(), sample(50_000u64, 33u64 * GIB));
    assert!(
        host_memory_line(&observation, Language::PtBr).contains(text(Language::PtBr).not_measured)
    );
    observation.status.remove("host_memory");
    assert!(
        host_memory_line(&observation, Language::PtBr).contains(text(Language::PtBr).not_measured)
    );
}

#[test]
fn compression_summary_keeps_only_complete_fields() {
    let mut observation = active_cache_observation();
    assert!(compression_line(&observation, Language::En, 80).is_none());
    observation.status.insert(
        "zram_compression".into(),
        json!({
            "orig_data_bytes": 2u64 * GIB,
            "compr_data_bytes": 1100u64 * MIB,
            "mem_used_bytes": 1120u64 * MIB,
            "huge_pages": 24500u64,
            "compressor": "lzo-rle"
        }),
    );
    let narrow = compression_line(&observation, Language::En, 38).unwrap();
    assert!(narrow.contains("1.86:1"));
    assert!(!narrow.contains("cost"));
    let wide = compression_line(&observation, Language::En, 120).unwrap();
    assert!(wide.contains("cost 1.1 GiB"));
    assert!(wide.contains("lzo-rle"));
    assert!(wide.contains("24500"));
    assert!(
        compression_line(&observation, Language::En, 0)
            .unwrap()
            .is_empty()
    );
    observation.status.insert(
        "zram_compression".into(),
        json!({
            "orig_data_bytes": u64::MAX,
            "compr_data_bytes": 0u64
        }),
    );
    let fallback = compression_line(&observation, Language::En, 20).unwrap();
    assert!(fallback.contains(text(Language::En).not_measured));
    assert!(Line::from(fallback.as_str()).width() <= 20);
}

#[test]
fn restart_hint_is_not_a_fake_live_verdict() {
    let observation = active_cache_observation();
    let backend = TestBackend::new(120, 36);
    let mut terminal = Terminal::new(backend).expect("terminal");
    terminal
        .draw(|frame| {
            draw(
                frame,
                &observation,
                &DashHistory::default(),
                &metadata(),
                Language::En,
                Screen::Details,
            )
        })
        .expect("draw");
    let buffer = terminal.backend().buffer();
    let width = buffer.area().width as usize;
    let rendered = buffer
        .content()
        .chunks(width)
        .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(rendered.contains("down --plan"), "{rendered}");
    assert!(
        !rendered.contains("SAFE"),
        "no live SAFE verdict: {rendered}"
    );
    assert!(
        !rendered.contains("BLOCKED"),
        "no live BLOCKED verdict: {rendered}"
    );
}

#[test]
fn sparkline_handles_absent_and_flat_series() {
    let empty = std::collections::VecDeque::new();
    assert_eq!(sparkline(&empty, 100, 10), "—");
    let flat = std::collections::VecDeque::from([0, 0, 0]);
    assert_eq!(sparkline(&flat, 100, 10), "▁▁▁");
    let extreme = std::collections::VecDeque::from([0, u64::MAX]);
    assert_eq!(sparkline(&extreme, u64::MAX, 10), "▁█");
    assert_eq!(sparkline(&extreme, u64::MAX, 0), "");
}

#[test]
fn bytes_keep_resolution_and_localize_decimal_separator() {
    assert_eq!(fmt_bytes(456 * KIB, Language::PtBr), "456,0 KiB");
    assert_eq!(fmt_bytes(33 * MIB, Language::En), "33.0 MiB");
}

#[test]
fn dashboard_does_not_invent_adapter_windows_or_cache_mode() {
    let observation = active_cache_observation();
    let mut history = DashHistory::default();
    history.push_observation(&observation);
    for (width, height) in [(80, 24), (120, 36)] {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
        terminal
            .draw(|frame| {
                draw(
                    frame,
                    &observation,
                    &history,
                    &metadata(),
                    Language::En,
                    Screen::Main,
                )
            })
            .expect("draw");
        let text = terminal
            .backend()
            .buffer()
            .content()
            .chunks(usize::from(width))
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!text.contains("RTX 2060"), "{text}");
        assert!(!text.contains("-5 min"), "{text}");
        assert!(!text.contains("RawOnly 1:1"), "{text}");
        assert!(text.contains("GPU"), "{text}");
        assert!(text.contains("RamShared swap"), "{text}");
    }
}

#[test]
fn unavailable_pressure_never_renders_fabricated_zero_percent() {
    let mut observation = active_cache_observation();
    observation.control_plane.memory_psi_available = false;
    let mut terminal = Terminal::new(TestBackend::new(120, 40)).expect("terminal");
    terminal
        .draw(|frame| {
            draw(
                frame,
                &observation,
                &DashHistory::default(),
                &metadata(),
                Language::En,
                Screen::Pressure,
            )
        })
        .expect("draw");
    let text = terminal
        .backend()
        .buffer()
        .content()
        .chunks(120)
        .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("unavailable"), "{text}");
    assert!(!text.contains("full  0.00%"), "{text}");
}

#[test]
fn main_dashboard_renders_live_idle_stale_and_unmeasured_gpu_busy() {
    let mut observation = active_cache_observation();
    observation.epoch_ms = 50_000;

    // 1. Live non-zero reading renders both EN and PT-BR with bar + mem%.
    observation.status.insert(
        "gpu_utilization".to_string(),
        json!({
            "schema_version": 1,
            "gpu_pct": 13,
            "memory_pct": 2,
            "sampled_at_unix_ms": 49_500
        }),
    );
    let en = gpu_busy_line(&observation, Language::En, 58);
    assert!(en.contains("GPU busy"), "{en}");
    assert!(en.contains("13%"), "{en}");
    assert!(en.contains("mem 2%"), "{en}");
    assert!(!en.contains("not measured"), "{en}");
    let pt = gpu_busy_line(&observation, Language::PtBr, 38);
    assert!(pt.contains("GPU ocupada"), "{pt}");
    assert!(pt.contains("13%"), "{pt}");
    assert!(pt.contains("mem 2%"), "{pt}");

    // Render full main screen at 120x36 to ensure no clipping or fallback text.
    let mut terminal = Terminal::new(TestBackend::new(120, 36)).expect("terminal");
    terminal
        .draw(|frame| {
            draw(
                frame,
                &observation,
                &DashHistory::default(),
                &metadata(),
                Language::En,
                Screen::Main,
            )
        })
        .expect("draw");
    let frame_text = terminal
        .backend()
        .buffer()
        .content()
        .chunks(120)
        .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(frame_text.contains("13% · mem 2%"), "{frame_text}");
    assert!(
        !frame_text.contains("GPU busy: not measured"),
        "{frame_text}"
    );

    // 2. Measured 0% renders an explicit 0%, never "not measured".
    observation.status.insert(
        "gpu_utilization".to_string(),
        json!({
            "schema_version": 1,
            "gpu_pct": 0,
            "memory_pct": 0,
            "sampled_at_unix_ms": 49_500
        }),
    );
    let idle = gpu_busy_line(&observation, Language::En, 58);
    assert!(idle.contains("0% · mem 0%"), "{idle}");

    // 3. Aged or refresh-failed reading renders stale without inventing 0%.
    observation.status.insert(
        "gpu_utilization".to_string(),
        json!({
            "schema_version": 1,
            "gpu_pct": 42,
            "memory_pct": 9,
            "sampled_at_unix_ms": 1_000
        }),
    );
    let aged = gpu_busy_line(&observation, Language::En, 58);
    assert_eq!(aged, " GPU busy: stale");
    let aged_pt = gpu_busy_line(&observation, Language::PtBr, 58);
    assert_eq!(
        aged_pt,
        format!(" GPU ocupada: {}", text(Language::PtBr).stale)
    );

    observation.status.insert(
        "gpu_utilization".to_string(),
        json!({
            "schema_version": 1,
            "gpu_pct": 42,
            "memory_pct": 9,
            "sampled_at_unix_ms": 49_500
        }),
    );
    observation.errors.push("sample_refresh_failed".to_string());
    assert_eq!(
        gpu_busy_line(&observation, Language::En, 58),
        " GPU busy: stale"
    );
    observation.errors.clear();

    // 4. Absent, null, or malformed samples render "not measured".
    observation
        .status
        .insert("gpu_utilization".to_string(), json!(null));
    assert_eq!(
        gpu_busy_line(&observation, Language::En, 58),
        " GPU busy: not measured"
    );
    observation.status.insert(
        "gpu_utilization".to_string(),
        json!({
            "schema_version": 1,
            "gpu_pct": 101,
            "memory_pct": 2,
            "sampled_at_unix_ms": 49_500
        }),
    );
    assert_eq!(
        gpu_busy_line(&observation, Language::En, 58),
        " GPU busy: not measured"
    );
    observation.status.remove("gpu_utilization");
    assert_eq!(
        gpu_busy_line(&observation, Language::PtBr, 58),
        format!(" {}", text(Language::PtBr).gpu_busy)
    );
}
