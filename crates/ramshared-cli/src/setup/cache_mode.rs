// SPDX-License-Identifier: MIT
// Copyright (c) 2024-2026 Emerson Busson. All rights reserved.
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/setup/cache_mode.rs
//! Stage-only next-boot cache profile custody (`independent` | `pooled`).
//! SPEC: the protocol specification §DT-1, §DT-10, §DT-11.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use ramshared_ipc::cache_wire::{CACHE_WIRE_VERSION, DEFAULT_POOLED_CACHE_PORT};
use serde::{Deserialize, Serialize};

use crate::cascade::ORIGIN_CONFIG_FILE;
use crate::cascade::boot::release_version;
use crate::customer::{CustomerError, CustomerReason, CustomerState};

pub(crate) const CACHE_MODE_PATH: &str = "/etc/ramshared/cache-mode.json";
const DEFAULT_KEY_REF: &str = "/etc/ramshared/pooled.key";
const SCHEMA_VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CacheMode {
    Independent,
    Pooled,
}

impl CacheMode {
    pub(crate) fn parse(raw: &str) -> Result<Self, CustomerError> {
        match raw.trim() {
            "independent" => Ok(Self::Independent),
            "pooled" => Ok(Self::Pooled),
            other => Err(err(
                CustomerReason::UnsupportedSurface,
                format!("unsupported cache mode `{other}` (expected `independent` or `pooled`)"),
            )),
        }
    }

    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Independent => "independent",
            Self::Pooled => "pooled",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StagedCacheMode {
    pub schema_version: u32,
    pub profile: CacheMode,
    pub role: String,
    pub endpoint_version: u8,
    pub data_port: u32,
    pub key_ref: String,
    pub release_binding: String,
    pub origin_binding: String,
    pub generation: u64,
}

#[derive(Clone, Debug)]
pub(crate) struct StageBinding {
    pub effective_uid: u32,
    pub target_path: PathBuf,
    pub active_release: String,
    pub expected_release: String,
    pub active_origin: String,
    pub expected_origin: String,
    pub role: String,
    pub key_ref: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct StageOutcome {
    pub next_boot_profile: CacheMode,
    pub generation: u64,
    pub reconciled_torn_temps: usize,
    pub storage_touched: bool,
    pub worker_started: bool,
}

fn err(reason: CustomerReason, detail: impl Into<String>) -> CustomerError {
    CustomerError {
        reason,
        detail: detail.into(),
        state: CustomerState::InstalledOff,
    }
}

pub(crate) fn parse_stage_args(args: &[String]) -> Result<(CacheMode, bool), CustomerError> {
    let Some((sub, rest)) = args.split_first() else {
        return Err(err(
            CustomerReason::UnsupportedSurface,
            "usage: ramshared setup cache-mode stage --mode independent|pooled [--json]",
        ));
    };
    if sub != "stage" {
        return Err(err(
            CustomerReason::UnsupportedSurface,
            format!("unsupported cache-mode subcommand `{sub}` (available: `stage`)"),
        ));
    }
    let mut mode = None;
    let mut json = false;
    let mut idx = 0;
    while idx < rest.len() {
        match rest[idx].as_str() {
            "--json" if !json => json = true,
            "--mode" if mode.is_none() && idx + 1 < rest.len() => {
                idx += 1;
                mode = Some(CacheMode::parse(&rest[idx])?);
            }
            other => {
                return Err(err(
                    CustomerReason::UnsupportedSurface,
                    format!("unsupported cache-mode stage argument: {other}"),
                ));
            }
        }
        idx += 1;
    }
    let mode = mode.ok_or_else(|| {
        err(
            CustomerReason::UnsupportedSurface,
            "cache-mode stage requires --mode independent|pooled",
        )
    })?;
    Ok((mode, json))
}

fn recovery_err(label: &str, e: impl std::fmt::Display) -> CustomerError {
    err(CustomerReason::RecoveryRequired, format!("{label}: {e}"))
}

pub(crate) fn reconcile_torn_temps(target: &Path) -> Result<usize, CustomerError> {
    let parent = target
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or_else(|| {
            err(
                CustomerReason::UnsupportedSurface,
                "missing parent directory",
            )
        })?;
    let name = target.file_name().and_then(|n| n.to_str()).ok_or_else(|| {
        err(
            CustomerReason::UnsupportedSurface,
            "invalid target filename",
        )
    })?;
    let prefix = format!(".{name}.tmp.");
    let mut cleaned = 0usize;
    let entries = fs::read_dir(parent).map_err(|e| recovery_err("read stage dir", e))?;
    for entry in entries.flatten() {
        if entry
            .file_name()
            .to_str()
            .is_some_and(|n| n.starts_with(&prefix))
            && fs::remove_file(entry.path()).is_ok()
        {
            cleaned += 1;
        }
    }
    Ok(cleaned)
}

pub(crate) fn stage_with_binding(
    mode: CacheMode,
    binding: &StageBinding,
) -> Result<StageOutcome, CustomerError> {
    if binding.effective_uid != 0 {
        return Err(err(
            CustomerReason::AuthorizationRequired,
            "staging next-boot cache mode requires root privileges",
        ));
    }
    if binding.active_release.trim().is_empty()
        || binding.active_release != binding.expected_release
        || binding.active_origin.trim().is_empty()
        || binding.active_origin != binding.expected_origin
    {
        return Err(err(
            CustomerReason::IdentityChanged,
            "stale release or origin binding refused before staging cache mode",
        ));
    }
    let reconciled = reconcile_torn_temps(&binding.target_path)?;
    let prev = match fs::read_to_string(&binding.target_path) {
        Ok(raw) => Some(
            serde_json::from_str::<StagedCacheMode>(&raw)
                .map_err(|e| recovery_err("existing cache-mode invalid", e))?,
        ),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(recovery_err("read cache-mode", e)),
    };
    let mut doc = StagedCacheMode {
        schema_version: SCHEMA_VERSION,
        profile: mode,
        role: binding.role.clone(),
        endpoint_version: CACHE_WIRE_VERSION,
        data_port: DEFAULT_POOLED_CACHE_PORT,
        key_ref: binding.key_ref.clone(),
        release_binding: binding.active_release.clone(),
        origin_binding: binding.active_origin.clone(),
        generation: 1,
    };
    if let Some(existing) = prev {
        if existing.profile == doc.profile
            && existing.role == doc.role
            && existing.release_binding == doc.release_binding
            && existing.origin_binding == doc.origin_binding
            && existing.key_ref == doc.key_ref
        {
            return Ok(StageOutcome {
                next_boot_profile: existing.profile,
                generation: existing.generation,
                reconciled_torn_temps: reconciled,
                storage_touched: false,
                worker_started: false,
            });
        }
        doc.generation = existing.generation.saturating_add(1);
    }
    commit_atomic(&binding.target_path, &doc)?;
    Ok(StageOutcome {
        next_boot_profile: doc.profile,
        generation: doc.generation,
        reconciled_torn_temps: reconciled,
        storage_touched: false,
        worker_started: false,
    })
}

fn commit_atomic(path: &Path, doc: &StagedCacheMode) -> Result<(), CustomerError> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or_else(|| {
            err(
                CustomerReason::UnsupportedSurface,
                "missing parent directory",
            )
        })?;
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| err(CustomerReason::UnsupportedSurface, "missing filename"))?;
    let tmp = parent.join(format!(".{name}.tmp.{}", std::process::id()));
    let bytes = serde_json::to_vec_pretty(doc).map_err(|e| recovery_err("encode", e))?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)
        .map_err(|e| recovery_err("create stage temp", e))?;
    let res = file
        .write_all(&bytes)
        .and_then(|()| file.sync_data())
        .and_then(|()| {
            drop(file);
            fs::rename(&tmp, path)
        })
        .and_then(|()| File::open(parent)?.sync_all());
    if let Err(e) = res {
        let _ = fs::remove_file(&tmp);
        return Err(recovery_err("commit stage file", e));
    }
    Ok(())
}

fn read_origin_binding_and_role(
    wsl_origin_path: &Path,
    native_origin_path: &Path,
) -> (String, String) {
    if let Ok(text) = fs::read_to_string(wsl_origin_path)
        && let Some(binding) = text.lines().find_map(|line| {
            let (k, v) = line.split_once('=')?;
            (k.trim() == "ORIGIN_PARTUUID" || k.trim() == "PARTUUID")
                .then(|| v.trim().trim_matches('"').to_string())
        })
        && !binding.is_empty()
    {
        return (binding, "wsl2".into());
    }
    if let Ok(text) = fs::read_to_string(native_origin_path)
        && let Ok(manifest) = ramshared_config::native_origin::NativeOriginManifest::parse(&text)
        && let ramshared_config::resource_profile::ResourceTarget::LinuxFileOrigin {
            filesystem_uuid,
            ..
        } = manifest.target
    {
        return (filesystem_uuid, "secondary_guest".into());
    }
    (String::new(), "wsl2".into())
}

pub(crate) fn run_with_binding(
    args: &[String],
    binding: &StageBinding,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> u8 {
    let (mode, json) = match parse_stage_args(args) {
        Ok(parsed) => parsed,
        Err(error) => {
            let _ = writeln!(stderr, "{error}");
            return error.exit_code();
        }
    };
    match stage_with_binding(mode, binding) {
        Ok(outcome) => {
            if json {
                if let Ok(encoded) = serde_json::to_string(&outcome) {
                    let _ = writeln!(stdout, "{encoded}");
                }
            } else {
                let _ = writeln!(
                    stdout,
                    "staged next-boot cache mode={} generation={} (storage unchanged)",
                    outcome.next_boot_profile.as_str(),
                    outcome.generation
                );
            }
            0
        }
        Err(error) => {
            let _ = writeln!(stderr, "{error}");
            error.exit_code()
        }
    }
}

pub(crate) fn run(args: &[String], stdout: &mut dyn Write, stderr: &mut dyn Write) -> u8 {
    let uid = rustix::process::geteuid().as_raw();
    let release = release_version();
    let native_path = ramshared_config::native_origin::default_native_origin_path();
    let (origin, role) = read_origin_binding_and_role(Path::new(ORIGIN_CONFIG_FILE), &native_path);
    let binding = StageBinding {
        effective_uid: uid,
        target_path: PathBuf::from(CACHE_MODE_PATH),
        active_release: release.clone(),
        expected_release: release,
        active_origin: origin.clone(),
        expected_origin: origin,
        role,
        key_ref: DEFAULT_KEY_REF.into(),
    };
    run_with_binding(args, &binding, stdout, stderr)
}

#[cfg(test)]
mod tests;
