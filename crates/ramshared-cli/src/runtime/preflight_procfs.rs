// SPDX-License-Identifier: MIT
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/runtime/preflight_procfs.rs
// Fixed-buffer /proc and statvfs observations (SPEC zero-script-native-runtime
// ITEM-2, RF-3, NFR-4). No subprocess, no shell, no unbounded read.

use std::fs::File;
use std::io::Read;
use std::path::Path;

pub const PROC_BUFFER_BYTES: usize = 16 * 1024;

#[derive(Debug)]
pub enum ProcfsError {
    Io(std::io::Error),
    TooLarge,
    Utf8,
    Malformed(&'static str),
}

impl std::fmt::Display for ProcfsError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "procfs io: {error}"),
            Self::TooLarge => formatter.write_str("procfs document exceeds the fixed buffer"),
            Self::Utf8 => formatter.write_str("procfs document is not UTF-8"),
            Self::Malformed(field) => write!(formatter, "procfs document is malformed: {field}"),
        }
    }
}

impl std::error::Error for ProcfsError {}

impl From<std::io::Error> for ProcfsError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct MemInfo {
    pub total_kib: Option<u64>,
    pub available_kib: Option<u64>,
    pub swap_total_kib: Option<u64>,
    pub swap_free_kib: Option<u64>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SwapEntry {
    pub filename: String,
    pub kind: String,
    pub size_kib: u64,
    pub used_kib: u64,
    pub priority: i32,
    pub dev: Option<u64>,
    pub ino: Option<u64>,
}

impl SwapEntry {
    /// Bind the parsed name to the real inode. A missing path is not a match;
    /// it is an absent identity, never a synthetic zero.
    pub fn bind_identity(&mut self) {
        let Ok(metadata) = std::fs::metadata(&self.filename) else {
            self.dev = None;
            self.ino = None;
            return;
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            self.dev = Some(metadata.dev());
            self.ino = Some(metadata.ino());
        }
        #[cfg(not(unix))]
        {
            let _ = metadata;
            self.dev = None;
            self.ino = None;
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, serde::Serialize)]
pub struct MemoryPressure {
    pub some_avg10: f32,
    pub some_avg60: f32,
    pub some_avg300: f32,
    pub some_total_us: u64,
    pub full_avg10: f32,
    pub full_avg60: f32,
    pub full_avg300: f32,
    pub full_total_us: u64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, serde::Serialize)]
pub struct StatVfs {
    pub fragment_size: u64,
    pub blocks: u64,
    pub free_blocks: u64,
    pub available_blocks: u64,
}

#[cfg(test)]
impl StatVfs {
    pub fn total_bytes(&self) -> Option<u64> {
        self.blocks.checked_mul(self.fragment_size)
    }

    pub fn available_bytes(&self) -> Option<u64> {
        self.available_blocks.checked_mul(self.fragment_size)
    }
}

/// Reusable 16 KiB reader. The buffer is allocated once and cleared per read;
/// a document that exceeds it is refused before a larger allocation exists.
pub struct ProcfsPreflight {
    buffer: Vec<u8>,
}

impl Default for ProcfsPreflight {
    fn default() -> Self {
        Self::new()
    }
}

impl ProcfsPreflight {
    pub fn new() -> Self {
        Self {
            buffer: Vec::with_capacity(PROC_BUFFER_BYTES),
        }
    }

    fn read_into_buffer(&mut self, path: &str) -> Result<&str, ProcfsError> {
        let file = File::open(path)?;
        self.buffer.clear();
        if self.buffer.capacity() < PROC_BUFFER_BYTES {
            self.buffer.reserve(PROC_BUFFER_BYTES);
        }
        let mut limited = file.take((PROC_BUFFER_BYTES + 1) as u64);
        limited.read_to_end(&mut self.buffer)?;
        if self.buffer.len() > PROC_BUFFER_BYTES {
            return Err(ProcfsError::TooLarge);
        }
        std::str::from_utf8(&self.buffer).map_err(|_| ProcfsError::Utf8)
    }

    #[cfg(test)]
    pub fn buffer_capacity(&self) -> usize {
        self.buffer.capacity()
    }

    #[cfg(test)]
    pub fn read_document(&mut self, path: &str) -> Result<&str, ProcfsError> {
        self.read_into_buffer(path)
    }

    pub fn read_meminfo(&mut self) -> Result<MemInfo, ProcfsError> {
        let text = self.read_into_buffer("/proc/meminfo")?;
        Ok(parse_meminfo(text))
    }

    pub fn read_swaps(&mut self) -> Result<Vec<SwapEntry>, ProcfsError> {
        let entries = {
            let text = self.read_into_buffer("/proc/swaps")?;
            parse_swaps_text(text)?
        };
        let mut entries = entries;
        for entry in &mut entries {
            entry.bind_identity();
        }
        Ok(entries)
    }

    pub fn read_pressure(&mut self) -> Result<MemoryPressure, ProcfsError> {
        let text = self.read_into_buffer("/proc/pressure/memory")?;
        parse_pressure(text)
    }
}

pub fn parse_meminfo(text: &str) -> MemInfo {
    let value = |name: &str| ramshared_tier::meminfo_counter_kib(text, name);
    MemInfo {
        total_kib: value("MemTotal"),
        available_kib: value("MemAvailable"),
        swap_total_kib: value("SwapTotal"),
        swap_free_kib: value("SwapFree"),
    }
}

pub fn parse_swaps_text(text: &str) -> Result<Vec<SwapEntry>, ProcfsError> {
    let mut lines = text.lines();
    let header = lines
        .next()
        .ok_or(ProcfsError::Malformed("missing swaps header"))?;
    if !header.starts_with("Filename") {
        return Err(ProcfsError::Malformed("unexpected swaps header"));
    }
    let mut entries = Vec::new();
    for line in lines {
        if line.trim().is_empty() {
            continue;
        }
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 5 {
            return Err(ProcfsError::Malformed("short swaps row"));
        }
        let size_kib = fields[2]
            .parse()
            .map_err(|_| ProcfsError::Malformed("swap size"))?;
        let used_kib = fields[3]
            .parse()
            .map_err(|_| ProcfsError::Malformed("swap used"))?;
        let priority = fields[4]
            .parse()
            .map_err(|_| ProcfsError::Malformed("swap priority"))?;
        entries.push(SwapEntry {
            filename: unescape_octal(fields[0]),
            kind: fields[1].to_string(),
            size_kib,
            used_kib,
            priority,
            dev: None,
            ino: None,
        });
    }
    Ok(entries)
}

pub fn parse_pressure(text: &str) -> Result<MemoryPressure, ProcfsError> {
    let mut pressure = MemoryPressure::default();
    let mut saw_some = false;
    let mut saw_full = false;
    for line in text.lines() {
        let mut fields = line.split_whitespace();
        let Some(kind) = fields.next() else {
            continue;
        };
        let mut avg10 = None;
        let mut avg60 = None;
        let mut avg300 = None;
        let mut total = None;
        for field in fields {
            if let Some(value) = field.strip_prefix("avg10=") {
                avg10 = value.parse::<f32>().ok();
            } else if let Some(value) = field.strip_prefix("avg60=") {
                avg60 = value.parse::<f32>().ok();
            } else if let Some(value) = field.strip_prefix("avg300=") {
                avg300 = value.parse::<f32>().ok();
            } else if let Some(value) = field.strip_prefix("total=") {
                total = value.parse::<u64>().ok();
            }
        }
        let (Some(avg10), Some(avg60), Some(avg300), Some(total)) = (avg10, avg60, avg300, total)
        else {
            return Err(ProcfsError::Malformed("pressure row"));
        };
        match kind {
            "some" => {
                pressure.some_avg10 = avg10;
                pressure.some_avg60 = avg60;
                pressure.some_avg300 = avg300;
                pressure.some_total_us = total;
                saw_some = true;
            }
            "full" => {
                pressure.full_avg10 = avg10;
                pressure.full_avg60 = avg60;
                pressure.full_avg300 = avg300;
                pressure.full_total_us = total;
                saw_full = true;
            }
            _ => {}
        }
    }
    if saw_some && saw_full {
        Ok(pressure)
    } else {
        Err(ProcfsError::Malformed("missing pressure rows"))
    }
}

/// Exact octal unescaping for `/proc/swaps` names. The kernel emits `\040`
/// for space, `\011` for tab, `\012` for newline and `\134` for backslash.
pub fn unescape_octal(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'\\' && index + 3 < bytes.len() {
            let digits = &bytes[index + 1..index + 4];
            if digits.iter().all(|digit| (b'0'..=b'7').contains(digit)) {
                let decoded = u16::from(digits[0] - b'0') * 64
                    + u16::from(digits[1] - b'0') * 8
                    + u16::from(digits[2] - b'0');
                if let Ok(byte) = u8::try_from(decoded) {
                    out.push(byte);
                    index += 4;
                    continue;
                }
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(unix)]
pub fn swap_entry_matches_path(entry: &SwapEntry, path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    entry.dev == Some(metadata.dev()) && entry.ino == Some(metadata.ino())
}

#[cfg(unix)]
pub fn statvfs(path: &Path) -> Result<StatVfs, ProcfsError> {
    let stats = rustix::fs::statvfs(path).map_err(|error| {
        ProcfsError::Io(std::io::Error::from_raw_os_error(error.raw_os_error()))
    })?;
    Ok(StatVfs {
        fragment_size: if stats.f_frsize == 0 {
            stats.f_bsize
        } else {
            stats.f_frsize
        },
        blocks: stats.f_blocks,
        free_blocks: stats.f_bfree,
        available_blocks: stats.f_bavail,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]
    #[test]
    fn native_memory_counters_refuse_wrong_units_and_duplicates() {
        for text in [
            "MemAvailable: 8192 MB\n",
            "MemAvailable: 8192\n",
            "MemAvailable: 8192 kB extra\n",
            "MemAvailable: 8192 kB\nMemAvailable: 4096 kB\n",
        ] {
            assert!(
                super::parse_meminfo(text).available_kib.is_none(),
                "{text:?}"
            );
        }
    }

    use super::*;

    #[test]
    fn proc_swaps_parser_unescapes_octal_paths_and_matches_inode() {
        let root = std::env::temp_dir().join(format!("ramshared-procfs-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("my swap.img");
        std::fs::write(&path, b"x").unwrap();
        let metadata = std::fs::metadata(&path).unwrap();
        use std::os::unix::fs::MetadataExt;
        let escaped = path.display().to_string().replace(' ', "\\040");
        let doc = format!("Filename\tType\tSize\tUsed\tPriority\n{escaped}\tfile\t1024\t512\t-2\n");
        let mut entries = parse_swaps_text(&doc).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].filename, path.display().to_string());
        entries[0].bind_identity();
        assert_eq!(entries[0].dev, Some(metadata.dev()));
        assert_eq!(entries[0].ino, Some(metadata.ino()));
        assert!(swap_entry_matches_path(&entries[0], &path));
        assert_eq!(unescape_octal(r"a\040b\011c\012d\134e"), "a b\tc\nd\\e");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn procfs_refuses_oversized_and_malformed_documents() {
        assert!(matches!(
            parse_swaps_text("not-a-header\n"),
            Err(ProcfsError::Malformed(_))
        ));
        assert!(matches!(
            parse_swaps_text("Filename\tType\tSize\tUsed\tPriority\nx file 1\n"),
            Err(ProcfsError::Malformed(_))
        ));
        assert!(matches!(
            parse_pressure("some avg10=0.00\n"),
            Err(ProcfsError::Malformed(_))
        ));
        let pressure = parse_pressure(
            "some avg10=0.25 avg60=0.10 avg300=0.05 total=42\nfull avg10=0.00 avg60=0.00 avg300=0.00 total=0\n",
        )
        .unwrap();
        assert_eq!(pressure.some_avg10, 0.25);
        assert_eq!(pressure.some_total_us, 42);
    }

    #[test]
    fn procfs_meminfo_preserves_absence_and_measured_zero() {
        let parsed = parse_meminfo("MemTotal: 1024 kB\nSwapTotal: 0 kB\nSwapFree: 0 kB\n");
        assert_eq!(parsed.total_kib, Some(1024));
        assert_eq!(parsed.available_kib, None);
        assert_eq!(parsed.swap_total_kib, Some(0));
        assert_eq!(parsed.swap_free_kib, Some(0));
    }
}

#[cfg(test)]
mod procfs_edge_tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn temp_root() -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        let root =
            std::env::temp_dir().join(format!("ramshared-procfs-edge-{}-{id}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn procfs_reads_live_documents_and_refuses_bad_inputs() {
        let mut preflight = ProcfsPreflight::new();
        assert!(preflight.buffer_capacity() >= PROC_BUFFER_BYTES);
        let mem = preflight.read_meminfo().unwrap();
        assert!(mem.total_kib.is_some());
        let _ = preflight.read_swaps().unwrap();
        if let Ok(pressure) = preflight.read_pressure() {
            assert!(pressure.some_avg10 >= 0.0);
        }
        assert!(
            preflight
                .read_document("/definitely/missing/procfs")
                .is_err()
        );

        let root = temp_root();
        let oversized = root.join("oversized");
        std::fs::write(&oversized, vec![b'A'; PROC_BUFFER_BYTES + 1]).unwrap();
        assert!(matches!(
            preflight.read_document(oversized.to_str().unwrap()),
            Err(ProcfsError::TooLarge)
        ));
        let invalid = root.join("invalid");
        std::fs::write(&invalid, [0xFF, 0xFE]).unwrap();
        assert!(matches!(
            preflight.read_document(invalid.to_str().unwrap()),
            Err(ProcfsError::Utf8)
        ));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn procfs_error_display_and_helpers_are_total() {
        for error in [
            ProcfsError::TooLarge,
            ProcfsError::Utf8,
            ProcfsError::Malformed("x"),
            ProcfsError::Io(std::io::Error::other("x")),
        ] {
            assert!(!error.to_string().is_empty());
        }
        let stats = StatVfs {
            fragment_size: 4096,
            blocks: 10,
            free_blocks: 4,
            available_blocks: 3,
        };
        assert_eq!(stats.total_bytes(), Some(10 * 4096));
        assert_eq!(stats.available_bytes(), Some(3 * 4096));
        let overflow = StatVfs {
            fragment_size: 2,
            blocks: u64::MAX,
            free_blocks: 0,
            available_blocks: u64::MAX,
        };
        assert_eq!(overflow.total_bytes(), None);
        assert_eq!(overflow.available_bytes(), None);
        assert_eq!(unescape_octal(r"a\q"), r"a\q");
        assert_eq!(unescape_octal("caf\u{00e9}\\040swap"), "caf\u{00e9} swap");
        assert_eq!(unescape_octal(r"\400"), r"\400");
        assert_eq!(unescape_octal(r"\777x"), r"\777x");
        assert!(matches!(
            parse_swaps_text("Filename\tType\tSize\tUsed\tPriority\nx file 1 2 nope\n"),
            Err(ProcfsError::Malformed(_))
        ));
        assert!(matches!(
            parse_pressure("some avg10=0 avg60=0 avg300=0 total=0\n"),
            Err(ProcfsError::Malformed(_))
        ));
        let entry = SwapEntry {
            filename: "/definitely/missing".to_string(),
            kind: "file".to_string(),
            size_kib: 1,
            used_kib: 0,
            priority: 0,
            dev: None,
            ino: None,
        };
        assert!(!swap_entry_matches_path(
            &entry,
            std::path::Path::new("/definitely/missing")
        ));
    }
}
