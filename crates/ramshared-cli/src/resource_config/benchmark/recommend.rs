// SPDX-License-Identifier: MIT
// Copyright (c) 2024-2026 Emerson Busson. All rights reserved.
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/resource_config/benchmark/recommend.rs
//! `ramshared setup recommend` — guided multi-disk discovery, speed benchmark,
//! and leader recommendation for Native Linux and WSL2 (`SPEC.md` DT-28).

use std::io::Write;
use std::path::Path;

use serde::Serialize;

use super::super::{BenchmarkAction, BenchmarkRequest, BenchmarkUseCase};
use super::{
    LatestBenchmarkDiagnostic, format_diagnostic_lines, read_latest_diagnostic_from,
    resolve_latest_benchmark_path,
};
use crate::resource_config::{
    InventoryError, ResourceSnapshot, RuntimePlatform, collect_snapshot, format_gib,
};

pub(crate) fn run_setup_recommend(
    args: &[String],
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> u8 {
    run_setup_recommend_with(
        args,
        &collect_snapshot,
        &mut |req, out| super::super::run(req, out),
        &resolve_latest_benchmark_path(),
        stdout,
        stderr,
    )
}

pub(crate) fn run_setup_recommend_with(
    args: &[String],
    collect_fn: &dyn Fn() -> Result<ResourceSnapshot, InventoryError>,
    run_bench: &mut dyn FnMut(BenchmarkRequest, &mut dyn Write) -> Result<(), InventoryError>,
    latest_path: &Path,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> u8 {
    let mut benchmark = false;
    let mut confirm = false;
    let mut use_case = BenchmarkUseCase::Swap;
    let mut volumes = Vec::new();
    let mut json = false;
    let mut idx = 0;

    while idx < args.len() {
        match args[idx].as_str() {
            "--benchmark" if !benchmark => benchmark = true,
            "--confirm" if !confirm => confirm = true,
            "--json" if !json => json = true,
            "--use-case" if idx + 1 < args.len() => {
                idx += 1;
                use_case = match args[idx].as_str() {
                    "swap" => BenchmarkUseCase::Swap,
                    "origin" => BenchmarkUseCase::Origin,
                    other => {
                        let _ = writeln!(stderr, "invalid --use-case value: {other}");
                        return 2;
                    }
                };
            }
            "--volumes" if idx + 1 < args.len() => {
                idx += 1;
                volumes = args[idx]
                    .split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(ToOwned::to_owned)
                    .collect();
            }
            other => {
                let _ = writeln!(stderr, "unsupported setup recommend argument: {other}");
                return 2;
            }
        }
        idx += 1;
    }

    if benchmark {
        let action = if confirm {
            BenchmarkAction::Confirmed
        } else {
            BenchmarkAction::Plan
        };
        let req = BenchmarkRequest {
            action,
            use_case,
            json,
            profile_path: None,
            batch: volumes,
        };
        return match run_bench(req, stdout) {
            Ok(()) => 0,
            Err(err) => {
                let _ = writeln!(stderr, "setup recommend benchmark failed: {err}");
                1
            }
        };
    }

    let snapshot = match collect_fn() {
        Ok(s) => s,
        Err(err) => {
            let _ = writeln!(stderr, "setup recommend discovery failed: {err}");
            return 1;
        }
    };
    let latest = read_latest_diagnostic_from(latest_path, &snapshot);

    if json {
        #[derive(Serialize)]
        struct RecommendDiscoveryJson<'a> {
            schema_version: u32,
            platform: RuntimePlatform,
            latest_benchmark: Option<&'a LatestBenchmarkDiagnostic>,
            windows_volumes: Option<&'a Vec<crate::resource_config::WindowsVolume>>,
        }
        let doc = RecommendDiscoveryJson {
            schema_version: 1,
            platform: snapshot.platform,
            latest_benchmark: latest.as_ref(),
            windows_volumes: snapshot.windows.as_ref().map(|w| &w.volumes),
        };
        match serde_json::to_string(&doc) {
            Ok(text) => {
                let _ = writeln!(stdout, "{text}");
                0
            }
            Err(err) => {
                let _ = writeln!(stderr, "cannot serialize recommendation JSON: {err}");
                1
            }
        }
    } else {
        let _ = writeln!(stdout, "RamShared storage discovery & recommendation:");
        if let Some(windows) = &snapshot.windows {
            for v in &windows.volumes {
                let drive = v.drive_letter.as_deref().unwrap_or("?");
                let label = v.label.as_deref().unwrap_or("unlabeled");
                let fs_name = v.file_system.as_deref().unwrap_or("unknown");
                let _ = writeln!(
                    stdout,
                    "  {drive}: ({label}, {fs_name}, {}) — free {} of {}",
                    v.drive_type,
                    format_gib(v.free_bytes),
                    format_gib(v.size_bytes),
                );
            }
        } else {
            for d in &snapshot.block_devices {
                let _ = writeln!(
                    stdout,
                    "  {} ({}, mounts: {}) — {}",
                    d.name,
                    format_gib(d.size_bytes),
                    if d.mountpoints.is_empty() {
                        "unmounted".into()
                    } else {
                        d.mountpoints.join(", ")
                    },
                    d.eligibility_reason
                );
            }
        }
        if let Some(diag) = latest.as_ref() {
            for line in format_diagnostic_lines(diag) {
                let _ = writeln!(stdout, "{line}");
            }
        } else {
            let _ = writeln!(
                stdout,
                "Speed diagnostic: not measured yet. Run `ramshared setup recommend --benchmark --confirm` to benchmark all eligible drives and record the winner."
            );
        }
        0
    }
}
