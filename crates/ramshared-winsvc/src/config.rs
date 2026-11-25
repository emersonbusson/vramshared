// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-winsvc/src/config.rs
//! WinDrive product configuration (SPEC windows-storport-cuda-vram DT-1 / DT-2).
//!
//! Closed storage-only shape: CUDA + queue + evidence; no pagefile/backend selector.

use std::path::{Path, PathBuf};

use serde::Deserialize;

/// Minimum LUN size (64 MiB) so three 4 KiB probe positions and GPT/NTFS are meaningful.
pub const MIN_SIZE_BYTES: u64 = 64 * 1024 * 1024;
/// Policy floor for CUDA free reserve (512 MiB).
pub const RESERVE_FLOOR_BYTES: u64 = 512 * 1024 * 1024;
/// Hard cap on mapped data area: `queue_depth * max_io_bytes` (4 MiB).
pub const MAX_DATA_AREA_BYTES: u64 = 4 * 1024 * 1024;
/// Max single I/O (1 MiB) — matches ABI `MAX_IO`.
pub const MAX_IO_BYTES_CAP: u32 = 1 << 20;
/// Max queue depth — matches ABI `MAX_QD`.
pub const MAX_QUEUE_DEPTH: u32 = 256;
/// Single-open config read cap (DT-1).
pub const MAX_CONFIG_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BrokerPipeV1 {
    NamedPipeV1,
}

/// Configuration for the Windows CUDA storage-only service.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WinDriveConfig {
    pub size_bytes: u64,
    pub block_size: u32,
    pub cuda_device: u32,
    /// Requested reserve; effective reserve is never below policy floor (DT-2).
    pub reserve_bytes: u64,
    pub queue_depth: u32,
    pub max_io_bytes: u32,
    /// Absolute evidence directory / JSONL path root (validated absolute).
    pub evidence_path: PathBuf,
    /// Drive letter for the storage-only volume (`D`..=`Z`).
    pub volume_letter: char,
    /// Optional private NTFS mount path used instead of an Explorer drive letter.
    #[serde(default)]
    pub volume_mount_path: Option<PathBuf>,
    pub broker_pipe: BrokerPipeV1,
    pub broker_ready_timeout_secs: u64,
    pub tenant: String,
    /// Heartbeat interval seconds (default 5).
    #[serde(default = "default_heartbeat_secs")]
    pub heartbeat_secs: u64,
}

fn default_heartbeat_secs() -> u64 {
    5
}

/// Configuration parse / validation error.
#[derive(Debug, PartialEq)]
pub enum ConfigError {
    Parse(String),
    Invalid { field: &'static str, detail: String },
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigError::Parse(s) => write!(f, "config parse: {s}"),
            ConfigError::Invalid { field, detail } => {
                write!(f, "config invalid {field}: {detail}")
            }
        }
    }
}

impl std::error::Error for ConfigError {}

/// TOML wrapper with a `[win_drive]` table.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Root {
    win_drive: WinDriveConfig,
}

impl WinDriveConfig {
    /// Parse TOML text containing a `[win_drive]` section.
    pub fn from_toml(text: &str) -> Result<Self, ConfigError> {
        if text.len() > MAX_CONFIG_BYTES {
            return Err(ConfigError::Invalid {
                field: "config",
                detail: format!("config exceeds {MAX_CONFIG_BYTES} bytes"),
            });
        }
        let root: Root = toml::from_str(text).map_err(|e| ConfigError::Parse(e.to_string()))?;
        root.win_drive.validate()?;
        Ok(root.win_drive)
    }

    /// Parse from an already-owned byte buffer (single-open path; DT-1).
    pub fn from_reader(buf: &[u8]) -> Result<Self, ConfigError> {
        if buf.len() > MAX_CONFIG_BYTES {
            return Err(ConfigError::Invalid {
                field: "config",
                detail: format!("config exceeds {MAX_CONFIG_BYTES} bytes"),
            });
        }
        let text = std::str::from_utf8(buf).map_err(|e| ConfigError::Parse(e.to_string()))?;
        Self::from_toml(text)
    }

    /// Validate invariants before provision (DT-2).
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.block_size != 512 && self.block_size != 4096 {
            return Err(ConfigError::Invalid {
                field: "block_size",
                detail: format!("must be 512 or 4096, got {}", self.block_size),
            });
        }
        if self.size_bytes < MIN_SIZE_BYTES {
            return Err(ConfigError::Invalid {
                field: "size_bytes",
                detail: format!("must be >= {MIN_SIZE_BYTES} (64 MiB)"),
            });
        }
        if usize::try_from(self.size_bytes).is_err() {
            return Err(ConfigError::Invalid {
                field: "size_bytes",
                detail: "must fit in usize on this host".into(),
            });
        }
        if !self.size_bytes.is_multiple_of(self.block_size as u64) {
            return Err(ConfigError::Invalid {
                field: "size_bytes",
                detail: "must be multiple of block_size".into(),
            });
        }
        if self.queue_depth == 0
            || self.queue_depth > MAX_QUEUE_DEPTH
            || !self.queue_depth.is_power_of_two()
        {
            return Err(ConfigError::Invalid {
                field: "queue_depth",
                detail: format!(
                    "must be power of two in 1..={MAX_QUEUE_DEPTH}, got {}",
                    self.queue_depth
                ),
            });
        }
        if self.max_io_bytes == 0 || self.max_io_bytes > MAX_IO_BYTES_CAP {
            return Err(ConfigError::Invalid {
                field: "max_io_bytes",
                detail: format!(
                    "must be non-zero and <= {MAX_IO_BYTES_CAP}, got {}",
                    self.max_io_bytes
                ),
            });
        }
        if !self.max_io_bytes.is_multiple_of(self.block_size) {
            return Err(ConfigError::Invalid {
                field: "max_io_bytes",
                detail: "must be multiple of block_size".into(),
            });
        }
        let data_area = (self.queue_depth as u64)
            .checked_mul(self.max_io_bytes as u64)
            .ok_or_else(|| ConfigError::Invalid {
                field: "queue_depth",
                detail: "queue_depth * max_io_bytes overflow".into(),
            })?;
        if data_area > MAX_DATA_AREA_BYTES {
            return Err(ConfigError::Invalid {
                field: "queue_depth",
                detail: format!(
                    "queue_depth * max_io_bytes = {data_area} exceeds {MAX_DATA_AREA_BYTES}"
                ),
            });
        }
        if !is_absolute_path(&self.evidence_path) {
            return Err(ConfigError::Invalid {
                field: "evidence_path",
                detail: "must be an absolute path".into(),
            });
        }
        let letter = self.volume_letter.to_ascii_uppercase();
        if !('D'..='Z').contains(&letter) {
            return Err(ConfigError::Invalid {
                field: "volume_letter",
                detail: format!("must be D..=Z, got {:?}", self.volume_letter),
            });
        }
        let prefix = r"C:\ProgramData\RamShared\mounts\";
        let is_valid_mount = self.volume_mount_path.as_ref().is_none_or(|path| {
            let value = path.to_string_lossy().replace('/', "\\");
            value
                .to_ascii_lowercase()
                .starts_with(&prefix.to_ascii_lowercase())
                && !value[prefix.len()..].is_empty()
                && !value.contains("..")
                && !value.contains(['\'', ';', '\r', '\n'])
        });
        if !is_valid_mount {
            return Err(ConfigError::Invalid {
                field: "volume_mount_path",
                detail: format!("must be a child of {prefix}"),
            });
        }
        if self.tenant.is_empty() {
            return Err(ConfigError::Invalid {
                field: "tenant",
                detail: "must be non-empty".into(),
            });
        }
        if !(1..=30).contains(&self.broker_ready_timeout_secs) {
            return Err(ConfigError::Invalid {
                field: "broker_ready_timeout_secs",
                detail: "must be in 1..=30".into(),
            });
        }
        Ok(())
    }

    /// Effective CUDA free reserve: `max(config, 512 MiB, ceil(total_vram/10))` (DT-2).
    pub fn effective_reserve_bytes(&self, total_vram: u64) -> u64 {
        let tenth = total_vram.div_ceil(10);
        self.reserve_bytes.max(RESERVE_FLOOR_BYTES).max(tenth)
    }

    /// Absolute evidence path as borrowed path.
    pub fn evidence_path(&self) -> &Path {
        &self.evidence_path
    }
}

/// Absolute path check that accepts Windows paths when validating on Linux hosts.
///
/// Product configs are authored for Windows (`C:\...`); CI unit tests run on Linux
/// and must still accept those absolute forms.
fn is_absolute_path(p: &Path) -> bool {
    if p.is_absolute() {
        return true;
    }
    let s = p.to_string_lossy();
    if s.starts_with(r"\\") || s.starts_with("//") {
        return true;
    }
    let b = s.as_bytes();
    b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/')
}

#[cfg(test)]
mod tests;
