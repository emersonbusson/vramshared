// SPDX-License-Identifier: MIT
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/customer/package_selector/sink_binding.rs
//! Retained lower-sink identity for validating a derived configuration.

use std::fs::File;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

/// Holds the sink directory, not permission to modify it or switch a release.
pub(in crate::customer) struct SinkWitness {
    path: String,
    directory: File,
    identity: String,
    device: u64,
    inode: u64,
    block_bytes: u64,
}

fn sample(path: &str, directory: &File) -> Option<(String, u64, u64, u64)> {
    let metadata = directory.metadata().ok()?;
    let mode = metadata.mode() & 0o7777;
    if !metadata.is_dir() || mode < 0o100 {
        return None;
    }
    let block_bytes = rustix::fs::fstatvfs(directory).ok()?.f_bsize;
    if !(512..=1_048_576).contains(&block_bytes) {
        return None;
    }
    // Match the installed binding contract; device number is excluded because
    // the same Windows-backed directory can report different device numbers.
    let text = format!(
        "{path}\0directory\0{}:{}:{}:{mode:o}:directory",
        metadata.ino(),
        metadata.uid(),
        metadata.gid()
    );
    let identity = crate::installed_image::sha256_reader(&mut text.as_bytes())?;
    Some((identity, metadata.dev(), metadata.ino(), block_bytes))
}

impl SinkWitness {
    pub(in crate::customer) fn observe(path: &str) -> Option<Self> {
        if !super::derived_config::safe_path(path) {
            return None;
        }
        let directory =
            crate::customer::transition_custody::open_directory_no_follow(Path::new(path), false)
                .ok()??;
        let (identity, device, inode, block_bytes) = sample(path, &directory)?;
        let witness = Self {
            path: path.to_owned(),
            directory,
            identity,
            device,
            inode,
            block_bytes,
        };
        witness.revalidate()?;
        Some(witness)
    }

    pub(super) fn revalidate(&self) -> Option<()> {
        let named = crate::customer::transition_custody::open_directory_no_follow(
            Path::new(&self.path),
            false,
        )
        .ok()??;
        let expected = (
            self.identity.clone(),
            self.device,
            self.inode,
            self.block_bytes,
        );
        (sample(&self.path, &self.directory)? == expected
            && sample(&self.path, &named)? == expected)
            .then_some(())
    }

    pub(in crate::customer) fn binding(&self) -> super::derived_config::Binding<'_> {
        super::derived_config::Binding {
            path: &self.path,
            identity: &self.identity,
            block_bytes: self.block_bytes,
        }
    }

    pub(in crate::customer) fn available_kib(&self) -> Option<u64> {
        self.revalidate()?;
        let stat = rustix::fs::fstatvfs(&self.directory).ok()?;
        if stat.f_frsize == 0 {
            return None;
        }
        stat.f_bavail
            .checked_mul(stat.f_frsize)
            .map(|bytes| bytes / 1024)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};

    struct Fixture(std::path::PathBuf);
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn holds_and_revalidates_the_real_sink_identity() {
        let path =
            std::env::temp_dir().join(format!("ramshared-sink-witness-{}", std::process::id()));
        std::fs::create_dir(&path).unwrap();
        let fixture = Fixture(path);
        let sink = fixture.0.join("sink");
        std::fs::create_dir(&sink).unwrap();
        std::fs::set_permissions(&sink, std::fs::Permissions::from_mode(0o700)).unwrap();
        let witness = SinkWitness::observe(sink.to_str().unwrap()).unwrap();
        assert!(witness.revalidate().is_some());
        let binding = witness.binding();
        assert_eq!(binding.path, sink.to_str().unwrap());
        assert_eq!(binding.identity.len(), 64);
        assert!(binding.block_bytes >= 512);
        std::fs::set_permissions(&sink, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(witness.revalidate().is_none());
        std::fs::set_permissions(&sink, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(witness.revalidate().is_some());
        std::fs::rename(&sink, fixture.0.join("original")).unwrap();
        std::fs::create_dir(&sink).unwrap();
        assert!(witness.revalidate().is_none());
        let link = fixture.0.join("link");
        symlink(&sink, &link).unwrap();
        assert!(SinkWitness::observe(link.to_str().unwrap()).is_none());
        assert!(SinkWitness::observe("/tmp/../tmp").is_none());
    }

    #[test]
    #[ignore = "read-only observation requires an installed bound lower sink"]
    fn live_installed_sink_matches_provenance() {
        let path =
            Path::new(crate::installed_image::PRODUCT_ROOT).join("current/INSTALL_PROVENANCE.json");
        let record: serde_json::Value =
            serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        let sink = &record["lower_sink"];
        let witness = SinkWitness::observe(sink["canonical_path"].as_str().unwrap()).unwrap();
        let binding = witness.binding();
        assert_eq!(Some(binding.identity), sink["identity_sha256"].as_str());
        assert_eq!(
            Some(binding.block_bytes),
            sink["filesystem_block_bytes"].as_u64()
        );
        assert!(witness.revalidate().is_some());
    }
}
