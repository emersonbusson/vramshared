// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/release_coherence/inventory.rs
//! Unit-directory presence and read failures for release fleet observation.
use super::ReleaseCoherenceError;
use std::path::Path;

/// Only a genuinely absent directory is an empty inventory. A symlink, a
/// non-directory or an unreadable directory cannot prove an empty fleet.
pub(super) fn read_unit_directory(
    path: &Path,
) -> Result<Option<std::fs::ReadDir>, ReleaseCoherenceError> {
    let failed = |reason: String| ReleaseCoherenceError::UnreadableUnit {
        unit: path.display().to_string(),
        reason,
    };
    match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(failed(error.to_string())),
        Ok(metadata) if !metadata.file_type().is_dir() => {
            return Err(failed("not a real unit directory".into()));
        }
        Ok(_) => {}
    }
    std::fs::read_dir(path)
        .map(Some)
        .map_err(|error| failed(error.to_string()))
}
