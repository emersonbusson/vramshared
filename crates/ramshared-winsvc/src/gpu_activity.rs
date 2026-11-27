// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-winsvc/src/gpu_activity.rs
//! Reduces Windows GPU Engine process instances to physical engine activity.

#[cfg(windows)]
mod native;
#[cfg(windows)]
pub(crate) mod sampler;

#[cfg(any(windows, test))]
mod counters {
    use ramshared_vram::host_gpu_activity::{
        HostGpuAdapterActivity, HostGpuEngineActivity, HostGpuProcessActivity,
        MAX_GPU_ENGINE_PROCESSES,
    };
    use std::collections::BTreeMap;

    fn hex_word(value: &str) -> Option<u32> {
        let digits = value.strip_prefix("0x")?;
        (digits.len() == 8)
            .then(|| u32::from_str_radix(digits, 16).ok())
            .flatten()
    }

    struct EngineInstance<'a> {
        adapter: (u32, u32, u32),
        engine: u32,
        pid: u32,
        kind: &'a str,
    }

    fn instance(name: &str) -> Option<EngineInstance<'_>> {
        let (pid, name) = name.strip_prefix("pid_")?.split_once("_luid_")?;
        let (luid, name) = name.split_once("_phys_")?;
        let (high, low) = luid.split_once('_')?;
        let (physical, name) = name.split_once("_eng_")?;
        let (engine, kind) = name.split_once("_engtype_")?;
        if kind.is_empty()
            || !kind
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        {
            return None;
        }
        Some(EngineInstance {
            adapter: (hex_word(high)?, hex_word(low)?, physical.parse().ok()?),
            engine: engine.parse().ok()?,
            pid: pid.parse().ok()?,
            kind,
        })
    }

    #[derive(Default)]
    struct EngineRows<'a> {
        kind: &'a str,
        by_process: BTreeMap<u32, f64>,
        ambiguous: bool,
    }

    fn ranked_processes(by_process: &BTreeMap<u32, f64>) -> Vec<HostGpuProcessActivity> {
        let mut ranked = Vec::<(u32, f64)>::with_capacity(MAX_GPU_ENGINE_PROCESSES);
        for (&pid, &rate) in by_process {
            if pid == 0 || rate.round() == 0.0 {
                continue;
            }
            // Map order supplies a stable PID tie-break for equal raw rates.
            let index = ranked.partition_point(|(_, existing)| *existing >= rate);
            if index < MAX_GPU_ENGINE_PROCESSES {
                if ranked.len() == MAX_GPU_ENGINE_PROCESSES {
                    ranked.pop();
                }
                ranked.insert(index, (pid, rate));
            }
        }
        ranked
            .into_iter()
            .map(|(pid, rate)| HostGpuProcessActivity {
                pid,
                busy_pct: rate.round() as u8,
            })
            .collect()
    }

    pub(super) fn summarize<'a>(
        rows: impl IntoIterator<Item = (&'a str, f64)>,
    ) -> Vec<HostGpuAdapterActivity> {
        let mut adapters = BTreeMap::<_, BTreeMap<_, EngineRows<'a>>>::new();
        for (name, busy_pct) in rows {
            let Some(EngineInstance {
                adapter,
                engine,
                pid,
                kind,
            }) = instance(name)
            else {
                continue;
            };
            if !(0.0..=100.0).contains(&busy_pct) {
                continue;
            }
            let engine = adapters
                .entry(adapter)
                .or_default()
                .entry(engine)
                .or_insert_with(|| EngineRows {
                    kind,
                    ..EngineRows::default()
                });
            let prior_rate = engine.by_process.insert(pid, busy_pct);
            // PDH can return the same instance more than once in a sample.
            engine.ambiguous |=
                engine.kind != kind || prior_rate.is_some_and(|prior| prior != busy_pct);
        }
        adapters
            .into_iter()
            .filter_map(|((high, low, physical_index), engines)| {
                let engines = engines
                    .into_iter()
                    .filter(|(_, rows)| !rows.ambiguous)
                    .map(|(index, rows)| HostGpuEngineActivity {
                        index,
                        kind: rows.kind.into(),
                        busy_pct: rows.by_process.values().sum::<f64>().round().min(100.0) as u8,
                        processes: Some(ranked_processes(&rows.by_process)),
                    })
                    .collect::<Vec<_>>();
                (!engines.is_empty()).then(|| HostGpuAdapterActivity {
                    luid: format!("{high:08x}:{low:08x}"),
                    physical_index,
                    engines,
                })
            })
            .collect()
    }
}

pub fn inspect_command(supervised: bool) -> i32 {
    #[cfg(windows)]
    let result = native::sample().and_then(|report| {
        use std::io::Write;
        let mut output = std::io::stdout().lock();
        if supervised {
            serde_json::to_writer(&mut output, &sampler::ProducerReport::new(report)?)
                .map_err(std::io::Error::other)?;
        } else {
            serde_json::to_writer(&mut output, &report).map_err(std::io::Error::other)?;
        }
        output.write_all(b"\n")
    });
    #[cfg(not(windows))]
    let _ = supervised;
    #[cfg(not(windows))]
    let result: std::io::Result<()> = Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "GPU Engine inspection requires Windows",
    ));
    match result {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("GPU activity inspection failed: {error}");
            2
        }
    }
}

#[cfg(test)]
mod tests {
    use super::counters::summarize;

    #[test]
    fn groups_processes_by_physical_engine() {
        let report = summarize([
            (
                "pid_1_luid_0x00000000_0x00001692_phys_0_eng_7_engtype_VideoEncode",
                10.25,
            ),
            (
                "pid_2_luid_0x00000000_0x00001692_phys_0_eng_7_engtype_VideoEncode",
                30.25,
            ),
            (
                "pid_1_luid_0x00000000_0x00001692_phys_0_eng_8_engtype_VideoEncode",
                0.0,
            ),
            (
                "pid_1_luid_0x00000000_0x00001692_phys_1_eng_7_engtype_VideoEncode",
                5.0,
            ),
            (
                "pid_1_luid_0x00000001_0x00001692_phys_0_eng_7_engtype_Copy",
                45.0,
            ),
        ]);
        assert_eq!(report.len(), 3);
        assert_eq!(report[0].luid, "00000000:00001692");
        assert_eq!(report[0].engines.len(), 2);
        assert_eq!(report[0].engines[0].index, 7);
        assert_eq!(report[0].engines[0].busy_pct, 41);
        assert_eq!(report[0].engines[1].busy_pct, 0);
        assert_eq!(report[1].physical_index, 1);
        assert_eq!(report[1].engines[0].busy_pct, 5);
        assert_eq!(report[2].engines[0].kind, "Copy");
        assert_eq!(report[2].engines[0].busy_pct, 45);
    }

    #[test]
    fn process_ranking_is_bounded_and_uses_engine_percentage() {
        let samples = [
            (80, 90.3),
            (17, 3.0),
            (22, 2.9),
            (9, 2.6),
            (3, 0.1),
            (0, 2.0),
        ]
        .map(|(pid, rate)| {
            (
                format!("pid_{pid}_luid_0x00000000_0x00001692_phys_0_eng_1_engtype_3D"),
                rate,
            )
        });
        let report = summarize(samples.iter().map(|(name, rate)| (name.as_str(), *rate)));
        let engine = &report[0].engines[0];
        assert_eq!(engine.busy_pct, 100);
        let ranked = engine
            .processes
            .iter()
            .flatten()
            .map(|row| (row.pid, row.busy_pct))
            .collect::<Vec<_>>();
        assert_eq!(ranked, [(80, 90), (17, 3), (22, 3)]);
    }

    #[test]
    fn identical_pdh_rows_keep_the_active_engine_and_count_each_process_once() {
        let idle = "pid_10_luid_0x00000000_0x00001692_phys_0_eng_7_engtype_VideoEncode";
        let busy = "pid_42_luid_0x00000000_0x00001692_phys_0_eng_7_engtype_VideoEncode";
        let report = summarize([(idle, 0.0), (busy, 49.75), (idle, 0.0), (busy, 49.75)]);
        assert_eq!(report.len(), 1, "keep the measured hardware adapter");
        let engines = &report[0].engines;
        assert_eq!(engines.len(), 1);
        assert_eq!(engines[0].kind, "VideoEncode");
        assert_eq!(engines[0].busy_pct, 50);
        let rows = engines[0]
            .processes
            .iter()
            .flatten()
            .map(|row| (row.pid, row.busy_pct))
            .collect::<Vec<_>>();
        assert_eq!(rows, [(42, 50)]);
    }

    #[test]
    fn rejects_invalid_rows_without_fabricating_idle() {
        let valid = "pid_1_luid_0x00000000_0x00000017_phys_0_eng_1_engtype_Copy";
        assert!(summarize([("not an engine", 0.0), (valid, f64::NAN), (valid, -1.0)]).is_empty());
        assert!(summarize([(valid, 1.0), (valid, 2.0)]).is_empty());
        assert_eq!(summarize([(valid, 0.0)])[0].engines[0].busy_pct, 0);
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "requires the host Windows GPU Engine counters"]
    fn native_activity_command_reports_observed_engine_scope() {
        assert_eq!(super::inspect_command(false), 0);
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "requires the host Windows GPU Engine counters"]
    fn native_activity_timestamp_is_between_utc_readings() -> Result<(), Box<dyn std::error::Error>>
    {
        use std::time::{SystemTime, UNIX_EPOCH};

        let before = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis();
        let sample = super::native::sample()?;
        let after = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis();
        assert!((before..=after).contains(&u128::from(sample.sampled_at_unix_ms)));
        Ok(())
    }
}
