// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/monitor/compact.rs
//! Compact single-line/two-line report, JSONL rotation, and private flight samples.

use super::*;

/// Formats cache occupancy with explicit VRAM-cache labels (DT-8).
///
/// Every figure here is cache occupancy in the worker's logical address space
/// or in provider VRAM. None of them is guest or host RAM, and no label may
/// suggest otherwise: the logical figure is always rendered as "VRAM cache
/// logical", never as memory available to the host.
///
/// The two store counters name **cache residency**, not a compression
/// mechanism. DT-9 compression is off by default and this tier stores raw
/// pages at 1:1, so "compressed payload" named a mechanism the tier does not
/// use — the same false noun `huge_pages` used before it was renamed to
/// `incompressible`. The wire keys stay `compressed_payload_bytes` /
/// `raw_payload_bytes` (they live in `ramshared-vram`); the operator-facing
/// noun names what the counter actually is: how many cached bytes are
/// resident in the encoded store versus the raw store. No compression ratio
/// is derivable from these two figures alone and none is printed.
///
/// Byte counts go through [`format_volume_bytes`], the monitor's one renderer
/// for a byte figure. An ad-hoc whole-MiB division collapsed a real 512 KiB
/// workspace to `0 MiB` — a present measurement printed as if the cache were
/// empty — and a second byte renderer is how that kind of drift gets in.
pub(super) fn format_cache_telemetry_labels(cache: &WorkerCacheTelemetry) -> String {
    let refusal = cache
        .codec
        .refusal_reason
        .as_deref()
        .map(|reason| format!(" ({reason})"))
        .unwrap_or_default();
    format!(
        "VRAM cache logical: {} | VRAM cache slabs: {} | codec workspace: {} | encoded cached: {} | raw cached: {} | raw bypass: {} | metadata: {} | codec: {}/{}{} | codec errors: integrity={} decode={} timeout={}",
        format_volume_bytes(cache.logical_cached_bytes).trim(),
        format_volume_bytes(cache.physical_cache_slab_bytes).trim(),
        format_volume_bytes(cache.codec_workspace_bytes).trim(),
        format_volume_bytes(cache.compressed_payload_bytes).trim(),
        format_volume_bytes(cache.raw_payload_bytes).trim(),
        format_volume_bytes(cache.raw_bypass_bytes).trim(),
        cache
            .metadata_bytes
            .map(|bytes| format_volume_bytes(bytes).trim().to_owned())
            .or_else(|| cache
                .device_metadata_lower_bound_bytes
                .map(|bytes| format!("at least {} (partial)", format_volume_bytes(bytes).trim())))
            .unwrap_or_else(|| "unavailable".into()),
        cache.codec.capability.as_str(),
        cache.codec.state.as_str(),
        refusal,
        cache.codec_integrity_errors,
        cache.codec_decode_errors,
        cache.codec_timeouts,
    )
}

/// Bytes in one KiB. The capacity document reports KiB; the one byte renderer
/// takes bytes. This is the only conversion between them in compact mode.
const BYTES_PER_KIB: u64 = 1024;

/// Render one compact-mode capacity cell from the document's KiB figure.
///
/// Routes through [`format_volume_bytes`], the monitor's one byte renderer.
/// The cache-telemetry line already documents why: "An ad-hoc whole-MiB
/// division collapsed a real 512 KiB workspace to `0 MiB` — a present
/// measurement printed as if the cache were empty — and a second byte renderer
/// is how that kind of drift gets in." Compact mode still had one. It divided
/// by 1024 and printed MiB, so every figure under one MiB read as zero.
///
/// `None` is "the capacity document did not carry this key" — not a measured
/// zero — and prints `unknown`.
pub(super) fn compact_capacity_cell(kib: Option<u64>) -> String {
    kib.map_or_else(
        || "unknown".to_string(),
        |kib| {
            format_volume_bytes(kib.saturating_mul(BYTES_PER_KIB))
                .trim()
                .to_string()
        },
    )
}

/// The compact face of one observation — both lines, as pure text.
///
/// Output rendering separated from the collect/print loop so the face is a
/// function of the observation alone. The freshness clock that ages the cache
/// sample is the frame's own [`Observation::epoch_ms`], never a wall-clock
/// read at print time: one frame carries one clock, and the dashboard's GPU
/// rows already age at that stamp. Asking the same question at two instants
/// is how the compact line and the panel could disagree about one sample.
pub(super) fn compact_report(observation: &Observation) -> String {
    let capacity = observation.value("capacity").and_then(Value::as_object);
    let number = |key: &str| {
        compact_capacity_cell(
            capacity
                .and_then(|value| value.get(key))
                .and_then(Value::as_u64),
        )
    };
    let memory_pressure = if observation.control_plane.memory_psi_available {
        format!("{:.2}%", observation.control_plane.memory_psi_full_avg10)
    } else {
        "unavailable".to_string()
    };
    // DT-8: cache occupancy is rendered on its own line with explicit
    // VRAM-cache labels, never merged into the guest/host memory figures.
    // The row owns its two refusals; see [`cache_telemetry_row`].
    let cache_line = cache_telemetry_row(
        observation.value("gpu_cache"),
        observation_sample_is_stale(observation),
        observation.epoch_ms,
    );
    // `gpu_headroom_kib` is the spendable remainder of the VRAM budget. It is
    // labelled "GPU headroom" and never "GPU reserve": in this product's
    // vocabulary the reserve is the untouchable floor the admission path
    // enforces (`vram_reserve_enforced_mib` on the GPU panel), and two faces
    // of one monitor must not use one name for two different quantities.
    format!(
        "VRAM cached: {} | GPU headroom: {} | SSD authoritative: {} | memory pressure: {} | state: {}\n{}",
        number("vram_cached_kib"),
        number("gpu_headroom_kib"),
        number("ssd_origin_written_kib"),
        memory_pressure,
        observation.string("overall_state"),
        cache_line
    )
}

pub(super) fn run_compact(options: &MonitorOptions) -> Result<(), MonitorError> {
    let benchmark_evidence = options.benchmark_evidence_path();
    loop {
        let observation = collect_observation(&benchmark_evidence)?;
        println!("{}", compact_report(&observation));
        if options.once {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(options.interval_ms));
    }
}

pub(super) fn run_jsonl(options: &MonitorOptions) -> Result<(), MonitorError> {
    let benchmark_evidence = options.benchmark_evidence_path();
    loop {
        let observation = collect_observation(&benchmark_evidence)?;
        if options.output.is_some() {
            crate::cascade::root_status_cache::publish_from_observation(
                &observation.status,
                observation.epoch_ms,
            );
        }
        let line = serde_json::to_string(&observation)
            .map_err(|error| MonitorError::Json(error.to_string()))?;
        if let Some(path) = &options.output {
            append_rotating(path, &line, max_log_bytes())?;
        }
        if let Some(path) = &options.heartbeat {
            write_atomic(path, &line)?;
        }
        println!("{line}");
        if options.once {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(options.interval_ms));
    }
}

pub(super) fn max_log_bytes() -> u64 {
    std::env::var("RAMSHARED_MONITOR_MAX_BYTES")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_MAX_LOG_BYTES)
}

pub(super) fn ensure_parent(path: &Path) -> Result<(), MonitorError> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent).map_err(|error| MonitorError::Io(error.to_string()))?;
    }
    Ok(())
}

pub(super) fn append_rotating(path: &Path, line: &str, max_bytes: u64) -> Result<(), MonitorError> {
    ensure_parent(path)?;
    if fs::metadata(path).is_ok_and(|metadata| metadata.len() >= max_bytes) {
        let rotated = PathBuf::from(format!("{}.1", path.to_string_lossy()));
        fs::rename(path, rotated).map_err(|error| MonitorError::Io(error.to_string()))?;
    }
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|error| MonitorError::Io(error.to_string()))?;
    writeln!(file, "{line}").map_err(|error| MonitorError::Io(error.to_string()))
}

/// Publish one line as a whole, private file.
///
/// The temp is created `0600` and `create_new`, then renamed into place. A
/// planted symlink at `path` is replaced by the rename rather than followed,
/// so this can never truncate a target chosen by someone else; a planted temp
/// name fails closed instead of being written through. The resulting file is
/// owner-readable only — the observation carries top-process names, cgroup
/// paths and adapter identity, and a world-readable copy of that is a
/// disclosure surface.
pub(super) fn write_atomic(path: &Path, line: &str) -> Result<(), MonitorError> {
    use std::os::unix::fs::OpenOptionsExt;

    ensure_parent(path)?;
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("ramshared-heartbeat.json");
    // Deterministic per process and leaf, so a caller can reason about it and
    // a stale name from this same process is the only thing that can collide.
    // `create_new` is `O_CREAT|O_EXCL`, which fails on a planted regular file
    // **or** a planted symlink instead of writing through either.
    let temporary = parent.join(format!(".{name}.{}.tmp", std::process::id()));
    let body = format!("{line}\n");
    let mut created = false;
    let written = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)
            .map_err(|error| MonitorError::Io(error.to_string()))?;
        created = true;
        file.write_all(body.as_bytes())
            .map_err(|error| MonitorError::Io(error.to_string()))?;
        fs::rename(&temporary, path).map_err(|error| MonitorError::Io(error.to_string()))
    })();
    // Only a temp this call created is ours to clean up. A planted name is
    // left exactly as it was found: the write refused it, and removing it
    // would be an effect on someone else's object.
    if written.is_err() && created {
        let _ = fs::remove_file(&temporary);
    }
    written
}

/// Leaf name of the live flight sample inside [`flight_sample_dir`].
pub(super) const FLIGHT_SAMPLE_NAME: &str = "flight-sample.json";

/// What a sample that did not land reports. One name for one fact: the sample
/// did not reach its reader, whatever the syscall said.
pub(super) const FLIGHT_SAMPLE_UNAVAILABLE: &str = "flight_sample_unavailable";

/// The directory the live flight sample lands in.
///
/// A per-user directory under `/dev/shm`, created `0700` and verified to be
/// owned by this process's user before anything is written into it. The
/// observation carries top-process names, cgroup paths and adapter identity;
/// leaving that under a fixed name in a world-writable sticky tmpfs was a
/// disclosure and symlink-redirect surface. A directory someone else owns is
/// refused rather than used, so the fix fails closed instead of writing into a
/// planted one. `None` means "no private directory is available", never "the
/// sample is empty".
pub(super) fn flight_sample_dir() -> Option<PathBuf> {
    use std::os::unix::fs::MetadataExt;

    let uid = fs::metadata("/proc/self").ok()?.uid();
    flight_sample_dir_in(Path::new("/dev/shm"), uid)
}

/// Private flight-sample directory under `shared_tmp`, owned by `uid`.
///
/// The base and uid are parameters so the create, already-exists, refused
/// create and foreign-owner arms are each reachable without depending on
/// what this host already has under `/dev/shm`.
pub(super) fn flight_sample_dir_in(shared_tmp: &Path, uid: u32) -> Option<PathBuf> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let dir = shared_tmp.join(format!("ramshared-{uid}"));
    match fs::create_dir(&dir) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(_) => return None,
    }
    let metadata = fs::metadata(&dir).ok()?;
    if metadata.uid() != uid {
        return None;
    }
    let mut permissions = metadata.permissions();
    permissions.set_mode(0o700);
    fs::set_permissions(&dir, permissions).ok()?;
    Some(dir)
}

/// Land one observation line where a stall reader can find it.
///
/// The write is [`write_atomic`]'s: private, whole, never through a symlink.
/// A failure is recorded on `observation.errors` — the sample did not land,
/// and swallowing that is how an unwritable target used to look identical to
/// a healthy one.
pub(super) fn publish_flight_sample(observation: &mut Observation, dir: Option<&Path>, line: &str) {
    let result = match dir {
        Some(dir) => write_atomic(&dir.join(FLIGHT_SAMPLE_NAME), line),
        None => Err(MonitorError::Io(
            "no private flight-sample directory".to_string(),
        )),
    };
    if result.is_err() {
        observation
            .errors
            .push(FLIGHT_SAMPLE_UNAVAILABLE.to_string());
    }
}
