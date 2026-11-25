// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/resource_config/gpu_inventory.rs
//! Bounded device-memory discovery and non-authoritative VRAM previews.

use std::collections::HashSet;
use std::fmt::Write as _;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use ramshared_vram::{
    GpuBudgetSnapshot, GpuBudgetSource, RUNTIME_FREE_BUFFER_BYTES, ReserveFloorPolicy,
};
use serde::Serialize;

pub(super) fn profile_key(uuid: &str) -> String {
    format!("nvidia-uuid:{}", uuid.to_ascii_lowercase())
}

/// CUDA and NVIDIA-SMI UUIDs are two representations of the same driver UUID.
/// Other API UUIDs need their own proven correspondence and are not guessed.
pub(crate) fn driver_profile_key(adapter: &ramshared_vram::GpuAdapterIdentity) -> Option<String> {
    adapter.resource_profile_key()
}

const MIB: u64 = 1024 * 1024;
const MAX_GPUS: usize = 16;
const OUTPUT_LIMIT: usize = 32 * 1024;
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(super) struct GpuObservation {
    pub(super) uuid: String,
    total_bytes: u64,
    free_bytes: u64,
    source: &'static str,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Preset {
    Conservative,
    Recommended,
}

impl Preset {
    pub(super) fn next(self) -> Self {
        match self {
            Self::Conservative => Self::Recommended,
            Self::Recommended => Self::Conservative,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Conservative => "conservative",
            Self::Recommended => "recommended",
        }
    }
}

pub(super) struct Preview {
    pub(super) cap_bytes: u64,
    pub(super) host_share_bytes: u64,
    pub(super) runtime_headroom_bytes: u64,
}

pub(super) fn parse_inventory(text: &str) -> Result<Vec<GpuObservation>, String> {
    let mut devices = Vec::new();
    let mut seen = HashSet::new();
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        if devices.len() == MAX_GPUS {
            return Err("GPU inventory exceeds the device limit".into());
        }
        let fields: Vec<_> = line.split(',').map(str::trim).collect();
        if fields.len() != 3 {
            return Err("GPU inventory row has an unexpected field count".into());
        }
        let uuid = fields[0];
        if uuid.len() <= 4
            || uuid.len() > 128
            || !uuid
                .get(..4)
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case("GPU-"))
            || !uuid
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
            || !seen.insert(uuid.to_ascii_lowercase())
        {
            return Err("GPU inventory has an invalid or duplicate UUID".into());
        }
        let parse_mib = |value: &str| {
            value
                .parse::<u64>()
                .ok()
                .and_then(|mib| mib.checked_mul(MIB))
        };
        let total_bytes = parse_mib(fields[1])
            .filter(|value| *value > 0)
            .ok_or("GPU inventory has an invalid total")?;
        let free_bytes = parse_mib(fields[2])
            .filter(|value| *value <= total_bytes)
            .ok_or("GPU inventory has an invalid free-memory value")?;
        devices.push(GpuObservation {
            uuid: uuid.into(),
            total_bytes,
            free_bytes,
            source: "nvidia_smi_device_memory",
        });
    }
    if devices.is_empty() {
        return Err("GPU inventory is empty".into());
    }
    Ok(devices)
}

pub(super) fn discover() -> Result<Vec<GpuObservation>, String> {
    discover_from_paths(&[
        Path::new("/usr/lib/wsl/lib/nvidia-smi"),
        Path::new("/usr/bin/nvidia-smi"),
    ])
}

fn discover_from_paths(paths: &[&Path]) -> Result<Vec<GpuObservation>, String> {
    let mut failure = None;
    for path in paths.iter().filter(|path| path.is_file()) {
        match query_device_memory(path) {
            Ok(devices) => return Ok(devices),
            Err(error) => failure = Some(error),
        }
    }
    failure.map_or_else(|| Ok(Vec::new()), Err)
}

fn query_device_memory(path: &Path) -> Result<Vec<GpuObservation>, String> {
    let mut command = Command::new(path);
    command.args([
        "--query-gpu=uuid,memory.total,memory.free",
        "--format=csv,noheader,nounits",
    ]);
    let output = crate::bounded_process::run_capture_command(
        &mut command,
        "GPU inventory",
        DISCOVERY_TIMEOUT,
        OUTPUT_LIMIT,
        |_| {},
    )
    .map_err(|error| error.to_string())?;
    if !output.status.success() {
        return Err("GPU inventory command failed".into());
    }
    let text = std::str::from_utf8(&output.stdout).map_err(|_| "GPU inventory is not UTF-8")?;
    parse_inventory(text)
}

pub(super) fn preview(device: &GpuObservation, preset: Preset) -> Preview {
    let policy = ReserveFloorPolicy::sealed();
    let host_share_bytes = policy.configured_reserve_bytes(device.total_bytes);
    // This is a device-memory estimate, never a trusted driver/WDDM admission.
    // `safe_target_bytes` uses only these byte fields; no admission is performed.
    let estimate = GpuBudgetSnapshot {
        adapter: None,
        total_bytes: Some(device.total_bytes),
        budget_bytes: device.total_bytes,
        used_bytes: device.total_bytes.saturating_sub(device.free_bytes),
        source: GpuBudgetSource::ProviderLocalEstimate,
        sampled_at: Instant::now(),
    };
    let maximum = estimate.safe_target_bytes(u64::MAX, host_share_bytes, RUNTIME_FREE_BUFFER_BYTES);
    let selected = match preset {
        Preset::Conservative => maximum / 2,
        Preset::Recommended => maximum,
    };
    Preview {
        cap_bytes: selected / MIB * MIB,
        host_share_bytes,
        runtime_headroom_bytes: RUNTIME_FREE_BUFFER_BYTES,
    }
}

pub(super) fn render_at(
    devices: &[GpuObservation],
    preset: Preset,
    observed_ms: u64,
    now_ms: u64,
) -> String {
    let mut output = String::new();
    if devices.is_empty() {
        return "GPU estimate unavailable; no adapter was measured.\n".into();
    }
    if observed_ms == 0
        || now_ms == 0
        || now_ms
            .checked_sub(observed_ms)
            .is_none_or(|age| age > ramshared_vram::GPU_BUDGET_MAX_AGE_MS)
    {
        return "GPU estimate is stale; refresh before reviewing these values.\n".into();
    }
    let _ = writeln!(
        output,
        "NVIDIA device-memory observation (not a WDDM budget):"
    );
    for (index, device) in devices.iter().enumerate() {
        let estimate = preview(device, preset);
        let filled =
            ((u128::from(estimate.cap_bytes) * 20) / u128::from(device.total_bytes)) as usize;
        let _ = writeln!(
            output,
            "GPU {} ({}): total {}, reported free {}",
            index + 1,
            device.uuid,
            super::format_gib(Some(device.total_bytes)),
            super::format_gib(Some(device.free_bytes))
        );
        let _ = writeln!(
            output,
            "  {} preview [{}{}] {}",
            preset.label(),
            "█".repeat(filled),
            "░".repeat(20 - filled),
            super::format_gib(Some(estimate.cap_bytes))
        );
        let _ = writeln!(
            output,
            "  Host/display share: {}; runtime headroom: {}.",
            super::format_gib(Some(estimate.host_share_bytes)),
            super::format_gib(Some(estimate.runtime_headroom_bytes))
        );
    }
    let _ = writeln!(
        output,
        "Device-memory estimate only: not a reservation or a driver-budget admission. Runtime checks may reduce it."
    );
    let _ = writeln!(output, "Preview only: no setting is saved or applied.");
    let _ = writeln!(
        output,
        "The host/display share is VRAM; emergency swap is separate disk capacity."
    );
    output
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    const MIB: u64 = 1024 * 1024;
    const EXAMPLE: &str = "GPU-1d3109d8-6193-e206-f283-3f99e0346db6, 6144, 6144\n";

    #[test]
    fn six_gib_example_keeps_two_gib_for_host_and_uses_no_more_than_four() {
        let devices = parse_inventory(EXAMPLE).unwrap();
        let recommended = preview(&devices[0], Preset::Recommended);
        let conservative = preview(&devices[0], Preset::Conservative);
        assert_eq!(recommended.host_share_bytes, 2048 * MIB);
        assert_eq!(recommended.cap_bytes, 3840 * MIB);
        assert_eq!(conservative.cap_bytes, 1920 * MIB);
        assert_eq!(
            recommended.runtime_headroom_bytes,
            RUNTIME_FREE_BUFFER_BYTES
        );
    }

    #[test]
    fn low_free_memory_reduces_preview_and_never_underflows() {
        let device = parse_inventory("GPU-a, 6144, 800\n").unwrap().remove(0);
        assert_eq!(preview(&device, Preset::Recommended).cap_bytes, 0);
        let device = parse_inventory("GPU-a, 6144, 100\n").unwrap().remove(0);
        assert_eq!(preview(&device, Preset::Recommended).cap_bytes, 0);
        let small = parse_inventory("GPU-b, 1024, 1024\n").unwrap().remove(0);
        assert_eq!(preview(&small, Preset::Recommended).cap_bytes, 0);
    }

    #[test]
    fn malformed_duplicate_and_unbounded_samples_never_form_a_preview() {
        for sample in [
            "",
            "GPU-a, 0, 0",
            "GPU-a, 100, 101",
            "GPU-a, N/A, 2",
            "GPU-a, 18446744073709551615, 0",
            "bad, 100, 20",
            "GPU-a, 3, 2, extra",
            "GPU-a, 3, 2\ngpu-A, 4, 2",
            "GPU-a, 3, 2\nGPU-a, 3, 2",
        ] {
            assert!(parse_inventory(sample).is_err(), "accepted {sample:?}");
        }
        let many = (0..17)
            .map(|n| format!("GPU-{n}, 6, 1\n"))
            .collect::<String>();
        assert!(parse_inventory(&many).is_err());
    }

    #[test]
    fn preview_switching_is_read_only_and_labels_estimates() {
        let devices = parse_inventory(EXAMPLE).unwrap();
        let text = render_at(&devices, Preset::Recommended, 1_000, 1_000);
        assert!(text.contains("3.75 GiB"));
        assert!(text.contains("2.00 GiB"));
        assert!(text.contains("estimate"));
        assert!(text.contains("not a reservation"));
        assert!(text.contains("no setting is saved or applied"));
        assert!(text.contains("emergency swap"));
        assert_eq!(Preset::Recommended.next(), Preset::Conservative);
        assert_eq!(Preset::Conservative.next(), Preset::Recommended);
        assert!(render_at(&[], Preset::Recommended, 1_000, 1_000).contains("unavailable"));
        assert!(render_at(&devices, Preset::Recommended, 1_000, 7_000).contains("stale"));
        assert!(render_at(&devices, Preset::Recommended, 2_000, 1_000).contains("stale"));
        assert!(render_at(&devices, Preset::Recommended, 0, 0).contains("stale"));
    }

    #[test]
    fn discovery_refuses_failed_or_empty_tools_and_handles_absence() {
        assert!(
            discover_from_paths(&[Path::new("/nonexistent-ramshared-gpu-tool")])
                .unwrap()
                .is_empty()
        );
        assert!(discover_from_paths(&[Path::new("/bin/false")]).is_err());
        assert!(
            discover_from_paths(&[Path::new("/bin/true")])
                .unwrap_err()
                .contains("empty")
        );
        assert!(
            discover_from_paths(&[Path::new("/bin/false"), Path::new("/bin/true")])
                .unwrap_err()
                .contains("empty")
        );
    }

    #[test]
    fn interactive_gpu_presets_change_only_the_preview() {
        let snapshot = super::super::ResourceSnapshot {
            platform: super::super::RuntimePlatform::NativeLinux,
            observed_unix_ms: super::super::unix_millis(),
            guest_observed_unix_ms: super::super::unix_millis(),
            gpu_observed_unix_ms: super::super::unix_millis(),
            guest_memory: super::super::MemorySnapshot {
                total_bytes: Some(16 * 1024 * MIB),
                available_bytes: Some(8 * 1024 * MIB),
                swap_total_bytes: Some(4 * 1024 * MIB),
                swap_free_bytes: Some(2 * 1024 * MIB),
                required_counters_available: true,
            },
            swaps: Vec::new(),
            block_devices: Vec::new(),
            windows: None,
            gpu_inventory: parse_inventory(EXAMPLE).unwrap(),
            gpu_budget_status: "not sampled".into(),
            warnings: Vec::new(),
            prepared_native_origin: super::super::prepared_origin::Observation::Absent,
        };
        let before = snapshot.clone();
        let backend = ratatui::backend::TestBackend::new(160, 48);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let now = super::super::unix_millis();
        let mut editor = super::super::tier_editor::Editor::from_snapshot(&snapshot, now);
        let mut show = |editor: &super::super::tier_editor::Editor| {
            terminal
                .draw(|frame| super::super::draw_config_frame(frame, &snapshot, 0, editor))
                .unwrap();
            terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>()
        };
        let recommended = show(&editor);
        assert!(recommended.contains("3.75 GiB"));
        editor.toggle_preset(now);
        let conservative = show(&editor);
        assert!(conservative.contains("1.88 GiB"));
        assert_eq!(
            snapshot, before,
            "preview selection must not edit the inventory or profile"
        );
    }
    #[test]
    fn cuda_uuid_profile_key_is_exact_and_other_identity_sources_refuse() {
        let mut adapter = ramshared_vram::GpuAdapterIdentity {
            node_mask: None,
            backend: "cuda".into(),
            key: "00112233445566778899AABBCCDDEEFF".into(),
            luid: None,
        };
        assert_eq!(
            driver_profile_key(&adapter).as_deref(),
            Some("nvidia-uuid:gpu-00112233-4455-6677-8899-aabbccddeeff")
        );
        adapter.backend = "vulkan".into();
        assert!(driver_profile_key(&adapter).is_none());
        adapter.backend = "cuda".into();
        for key in [
            "0",
            "00000000000000000000000000000000",
            "luid:12345678:00000001",
            "00112233445566778899aabbccddeefg",
            "éééééééééééééééé",
        ] {
            adapter.key = key.into();
            assert!(driver_profile_key(&adapter).is_none());
        }
    }
}
