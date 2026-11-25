// SPDX-License-Identifier: MIT
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/customer/package_source/stream.rs
//! Fixed-buffer verification of an approved source bundle.

mod cgroup;
mod guest;
mod memory;
pub(in crate::customer) use memory::{MemoryObservation, MemoryRequirement};

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};

use super::{
    DeclaredBundle, MEMBER_LIMIT, binary_header_matches, digest, observed_host_architecture,
};
use crate::customer::package::{ManifestArchitecture, ManifestTreeState};

/// A streamed witness borrows its exact source and its verification limits.
pub(in crate::customer) struct StreamVerifiedBundle<'a> {
    manifest_sha256: String,
    source: Option<&'a DeclaredBundle>,
    max_member_bytes: u64,
    max_total_bytes: u64,
    architecture: Option<ManifestArchitecture>,
    requirement: Option<MemoryRequirement>,
    release_identity: Option<String>,
}

impl<'a> StreamVerifiedBundle<'a> {
    pub(in crate::customer) fn verification_limits(&self) -> (u64, u64) {
        (self.max_member_bytes, self.max_total_bytes)
    }
    pub(in crate::customer) fn manifest_sha256(&self) -> &str {
        &self.manifest_sha256
    }

    pub(in crate::customer) fn source(&self) -> Option<&'a DeclaredBundle> {
        self.source
    }

    pub(in crate::customer) fn revalidate(
        &self,
        observe: &mut dyn FnMut() -> Option<MemoryObservation>,
    ) -> Option<()> {
        let Some(source) = self.source else {
            #[cfg(test)]
            return Some(());
            #[cfg(not(test))]
            return None;
        };
        let mut check = || self.check_fresh_memory(observe);
        let current = source.verify_streamed_inner(
            self.max_member_bytes,
            self.max_total_bytes,
            self.architecture?,
            self.requirement,
            &mut check,
        )?;
        (current.release_identity == self.release_identity).then_some(())
    }

    /// Return the identity only after rechecking the complete declared bundle
    /// and its matching signature. This remains a point-in-time observation.
    pub(in crate::customer) fn authenticated_release_identity(
        &self,
        signature: &crate::customer::package_trust::VerifiedSignature,
        observe: &mut dyn FnMut() -> Option<MemoryObservation>,
    ) -> Option<&str> {
        if signature.manifest_sha256() != self.manifest_sha256 {
            return None;
        }
        signature.revalidate()?;
        self.revalidate(observe)?;
        signature.revalidate()?;
        self.release_identity.as_deref()
    }

    pub(in crate::customer) fn check_fresh_memory(
        &self,
        observe: &mut dyn FnMut() -> Option<MemoryObservation>,
    ) -> Option<()> {
        match self.requirement {
            Some(requirement) => check_memory(&requirement, observe),
            None => {
                #[cfg(test)]
                {
                    Some(())
                }
                #[cfg(not(test))]
                {
                    None
                }
            }
        }
    }

    #[cfg(test)]
    pub(in crate::customer) fn fixture(manifest_sha256: String) -> Self {
        Self {
            manifest_sha256,
            source: None,
            max_member_bytes: 0,
            max_total_bytes: 0,
            architecture: None,
            requirement: None,
            release_identity: Some("fixture-release".into()),
        }
    }
}

fn check_memory(
    requirement: &MemoryRequirement,
    observe: &mut dyn FnMut() -> Option<MemoryObservation>,
) -> Option<()> {
    let observation = observe()?;
    let now = u64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_millis(),
    )
    .ok()?;
    requirement.admits_at(&observation, now).then_some(())
}

impl DeclaredBundle {
    /// Verify the declared set with fixed-size hashing buffers and explicit
    /// byte limits. This is not an immutable snapshot or a publisher signature.
    pub(in crate::customer) fn verify_streamed(
        &self,
        max_member_bytes: u64,
        max_total_bytes: u64,
        requirement: &MemoryRequirement,
        observe: &mut dyn FnMut() -> Option<MemoryObservation>,
    ) -> Option<StreamVerifiedBundle<'_>> {
        let mut check = || check_memory(requirement, observe);
        self.verify_streamed_inner(
            max_member_bytes,
            max_total_bytes,
            observed_host_architecture()?,
            Some(*requirement),
            &mut check,
        )
    }

    #[cfg(test)]
    pub(in crate::customer) fn verify_streamed_for(
        &self,
        max_member_bytes: u64,
        max_total_bytes: u64,
        architecture: ManifestArchitecture,
    ) -> Option<StreamVerifiedBundle<'_>> {
        self.verify_streamed_inner(
            max_member_bytes,
            max_total_bytes,
            architecture,
            None,
            &mut || Some(()),
        )
    }

    fn verify_streamed_inner(
        &self,
        max_member_bytes: u64,
        max_total_bytes: u64,
        architecture: ManifestArchitecture,
        requirement: Option<MemoryRequirement>,
        check: &mut dyn FnMut() -> Option<()>,
    ) -> Option<StreamVerifiedBundle<'_>> {
        check()?;
        if self.payload.architecture != architecture {
            return None;
        }
        let mut total = 0u64;
        for role in self.stage_roles()? {
            check()?;
            let (destination, _, expected) = self.publication_member(&role)?;
            let mut file = if destination == "SHA256SUMS" {
                None
            } else {
                Some(self.source.open_member(&destination)?)
            };
            let size = match file.as_ref() {
                Some(file) => file.metadata().ok()?.len(),
                None => u64::try_from(self.source.manifest.len()).ok()?,
            };
            if size > (MEMBER_LIMIT as u64).min(max_member_bytes) {
                return None;
            }
            total = total.checked_add(size)?;
            if total > max_total_bytes {
                return None;
            }
            if let Some(file) = file.as_mut() {
                if crate::installed_image::sha256_reader_with_limit(file, size).as_deref()
                    != Some(expected.as_str())
                    || file.metadata().ok()?.len() != size
                {
                    return None;
                }
            } else if digest(&self.source.manifest).as_deref() != Some(expected.as_str()) {
                return None;
            }
        }
        for role in ["ramshared", "ramsharedd"] {
            check()?;
            let (_, mode, _) = self.publication_member(role)?;
            let bytes = self.verified_source_prefix(role, 64, false)?;
            if mode & 0o111 == 0 || !binary_header_matches(&bytes, architecture) {
                return None;
            }
        }
        let mut line = |role: &str| {
            check()?;
            let (destination, _, _) = self.publication_member(role)?;
            if destination != role {
                return None;
            }
            let bytes = self.verified_source_prefix(
                role,
                crate::release_identity::MAX_IDENTITY_LINE_BYTES,
                true,
            )?;
            crate::release_identity::parse_identity_line(&bytes)
        };
        let release_identity = line("RELEASE_VERSION")?;
        let tree = match self.payload.source.tree_state {
            ManifestTreeState::Clean => "clean",
            ManifestTreeState::Dirty => "dirty",
        };
        if line("SOURCE_COMMIT").as_deref() != Some(self.payload.source.commit.as_str())
            || line("SOURCE_BRANCH").as_deref() != Some(self.payload.source.branch.as_str())
            || line("SOURCE_TREE_STATE").as_deref() != Some(tree)
        {
            return None;
        }
        check()?;
        Some(StreamVerifiedBundle {
            manifest_sha256: self.source.manifest_sha256.clone(),
            source: Some(self),
            max_member_bytes,
            max_total_bytes,
            architecture: Some(architecture),
            requirement,
            release_identity: Some(release_identity),
        })
    }

    fn verified_source_prefix(&self, role: &str, limit: usize, whole: bool) -> Option<Vec<u8>> {
        let (path, _, expected) = self.publication_member(role)?;
        let mut file = self.source.open_member(&path)?;
        let size = file.metadata().ok()?.len();
        if size > MEMBER_LIMIT as u64 || (whole && size > limit as u64) {
            return None;
        }
        let verify = |file: &mut File| {
            file.seek(SeekFrom::Start(0)).ok()?;
            (crate::installed_image::sha256_reader_with_limit(file, size).as_deref()
                == Some(expected.as_str()))
            .then_some(())
        };
        verify(&mut file)?;
        file.seek(SeekFrom::Start(0)).ok()?;
        let read_size = usize::try_from(size.min(limit as u64)).ok()?;
        let mut bytes = Vec::new();
        bytes.try_reserve_exact(read_size).ok()?;
        bytes.resize(read_size, 0);
        file.read_exact(&mut bytes).ok()?;
        if file.metadata().ok()?.len() != size {
            return None;
        }
        verify(&mut file)?;
        Some(bytes)
    }
}

#[cfg(test)]
mod shared_memory_tests {
    #[test]
    fn installer_uses_the_shared_headroom_contract() {
        use ramshared_tier::memory_headroom::{HostScope, MemorySample, shared_headroom};
        let guest = MemorySample {
            total_bytes: 100,
            available_bytes: 80,
            sampled_at_unix_ms: 1_000,
        };
        assert_eq!(
            shared_headroom(guest, HostScope::Native, Some(&[]), 10, 0, 1_001, 5),
            Some(70)
        );
        assert_eq!(
            shared_headroom(guest, HostScope::Native, None, 10, 0, 1_001, 5),
            None
        );
    }
}
