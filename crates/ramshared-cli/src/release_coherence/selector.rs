// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/release_coherence/selector.rs
//! Distinguish a genuinely absent release selector from unreadable identity.
use std::path::Path;

pub(super) fn selected_release_observation(product_root: &Path) -> Result<Option<String>, String> {
    let selector = product_root.join("current");
    match std::fs::symlink_metadata(&selector) {
        Ok(_) => crate::release_identity::read_release_version(&selector)
            .map(Some)
            .ok_or_else(|| "selected release identity is unreadable or invalid".into()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("cannot inspect release selector: {error}")),
    }
}

/// Refuse a changed, missing or unreadable selected release before an effect.
pub(super) fn require_selection(product_root: &Path, expected: &str) -> Result<(), String> {
    match selected_release_observation(product_root)? {
        Some(actual) if actual == expected => Ok(()),
        _ => Err("selected release changed or disappeared".into()),
    }
}
