// SPDX-License-Identifier: MIT
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/customer/package_source.rs
//! Descriptor-bound reads from an independently approved bundle manifest.
//!
//! This is input custody only. It does not execute a member or admit an install.

pub(super) mod stream;
pub(super) use stream::StreamVerifiedBundle;

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use rustix::fs::{Mode, OFlags, openat};

use super::transition::open_directory_no_follow;
use crate::release_identity::open_regular_at;

const MANIFEST_LIMIT: usize = 1024 * 1024;
pub(super) const MEMBER_LIMIT: usize = 256 * 1024 * 1024;

pub(super) struct ApprovedBundle {
    root: File,
    members: BTreeMap<String, String>,
    manifest: Vec<u8>,
    manifest_sha256: String,
}

/// An approved bundle bound to its strictly parsed payload declaration.
pub(super) struct DeclaredBundle {
    source: ApprovedBundle,
    payload: super::package::PayloadManifest,
}

#[cfg(test)]
pub(super) struct VerifiedMember {
    pub(super) destination: String,
    pub(super) mode: String,
    pub(super) bytes: Vec<u8>,
}

#[cfg(test)]
pub(super) struct VerifiedBundle {
    manifest_sha256: String,
    members: BTreeMap<String, VerifiedMember>,
}

#[cfg(test)]
impl VerifiedBundle {
    pub(super) fn manifest_sha256(&self) -> &str {
        &self.manifest_sha256
    }
}

impl DeclaredBundle {
    pub(super) fn source_identity(&self) -> Option<(u64, u64)> {
        let metadata = self.source.root.metadata().ok()?;
        (metadata.is_dir() && metadata.ino() != 0).then_some((metadata.dev(), metadata.ino()))
    }

    pub(super) fn open_verified(
        path: &Path,
        signature: &super::package_trust::VerifiedSignature,
    ) -> Option<Self> {
        Self::from_source(ApprovedBundle::open_verified(path, signature)?)
    }
    pub(super) fn manifest_sha256(&self) -> &str {
        &self.source.manifest_sha256
    }

    pub(super) fn open(path: &Path, approved_manifest_sha256: &str) -> Option<Self> {
        Self::from_source(ApprovedBundle::open(path, approved_manifest_sha256)?)
    }

    fn from_source(source: ApprovedBundle) -> Option<Self> {
        let bytes = source.member(
            "release-manifest.json",
            super::package::MAX_PAYLOAD_MANIFEST_BYTES,
        )?;
        let payload = super::package::PayloadManifest::parse(&bytes).ok()?;
        Some(Self { source, payload })
    }

    /// Resolve a declared role using the shared package destination contract.
    pub(super) fn member(&self, role: &str, limit: usize) -> Option<Vec<u8>> {
        let declaration = self
            .payload
            .contents
            .iter()
            .find(|member| member.role == role)?;
        let path = super::package_member::bundle_destination(declaration).ok()??;
        let bytes = self.source.member(&path, limit)?;
        if !declaration.kind.is_generated() {
            self.payload.verify_member_bytes(role, &bytes).ok()?;
        }
        Some(bytes)
    }

    /// Resolve a role only when its two declarations agree.
    pub(super) fn member_digest(&self, role: &str) -> Option<&str> {
        let declaration = self
            .payload
            .contents
            .iter()
            .find(|member| member.role == role)?;
        let path = super::package_member::bundle_destination(declaration).ok()??;
        if path == "SHA256SUMS" && declaration.kind.is_generated() {
            return Some(&self.source.manifest_sha256);
        }
        let outer = self.source.members.get(&path)?;
        if !declaration.kind.is_generated() && declaration.sha256.as_ref() != Some(outer) {
            return None;
        }
        Some(outer)
    }

    /// Resolve the exact bundle destination, installed mode and approved digest.
    pub(super) fn publication_member(&self, role: &str) -> Option<(String, u32, String)> {
        let member = self
            .payload
            .contents
            .iter()
            .find(|member| member.role == role)?;
        let destination = super::package_member::bundle_destination(member).ok()??;
        let mode = u32::from_str_radix(&member.mode, 8).ok()?;
        Some((destination, mode, self.member_digest(role)?.into()))
    }

    /// Observe the size of a declared member without retaining its contents.
    pub(super) fn member_size(&self, role: &str) -> Option<u64> {
        self.member_digest(role)?;
        let declaration = self
            .payload
            .contents
            .iter()
            .find(|member| member.role == role)?;
        let path = super::package_member::bundle_destination(declaration).ok()??;
        if path == "SHA256SUMS" {
            return u64::try_from(self.source.manifest.len()).ok();
        }
        Some(self.source.open_member(&path)?.metadata().ok()?.len())
    }

    /// Stage a declared member without retaining its body in process memory.
    pub(super) fn stage_member(
        &self,
        role: &str,
        slot: &mut super::package_stage::StageSlot,
        limit: u64,
    ) -> Result<u64, super::package_stage::StageError> {
        use super::package_stage::StageError;
        let declaration = self
            .payload
            .contents
            .iter()
            .find(|member| member.role == role)
            .ok_or(StageError::Refused)?;
        let path = super::package_member::bundle_destination(declaration)
            .map_err(|_| StageError::Refused)?
            .ok_or(StageError::Refused)?;
        let expected = self.member_digest(role).ok_or(StageError::Refused)?;
        if path == "SHA256SUMS" {
            return super::package_stage::copy_verified_bytes(
                &self.source.manifest,
                expected,
                slot,
                limit,
            );
        }
        self.source.copy_member_to(&path, slot, limit)
    }

    /// Continue a recorded stage only if its bytes are a prefix of the approved member.
    pub(super) fn resume_member(
        &self,
        role: &str,
        slot: &mut super::package_stage::StageSlot,
        limit: u64,
    ) -> Result<u64, super::package_stage::StageError> {
        use super::package_stage::StageError;
        let declaration = self
            .payload
            .contents
            .iter()
            .find(|member| member.role == role)
            .ok_or(StageError::Refused)?;
        let path = super::package_member::bundle_destination(declaration)
            .map_err(|_| StageError::Refused)?
            .ok_or(StageError::Refused)?;
        let expected = self.member_digest(role).ok_or(StageError::Refused)?;
        if path == "SHA256SUMS" {
            return super::package_stage::resume_verified_bytes(
                &self.source.manifest,
                expected,
                slot,
                limit,
            );
        }
        let file = self.source.open_member(&path).ok_or(StageError::Refused)?;
        super::package_stage::resume_verified(file, expected, slot, limit)
    }

    /// Derive the complete bundle role set from the two agreeing inventories.
    pub(super) fn stage_roles(&self) -> Option<BTreeSet<String>> {
        let mut destinations = BTreeSet::new();
        let mut roles = BTreeSet::new();
        let mut checksum_declared = false;
        for declaration in &self.payload.contents {
            let Some(path) = super::package_member::bundle_destination(declaration).ok()? else {
                continue;
            };
            if path == "SHA256SUMS" {
                if !declaration.kind.is_generated() || checksum_declared {
                    return None;
                }
                checksum_declared = true;
            } else if !destinations.insert(path) {
                return None;
            }
            self.member_digest(&declaration.role)?;
            if !roles.insert(declaration.role.clone()) {
                return None;
            }
        }
        if !checksum_declared
            || !destinations.contains("release-manifest.json")
            || !destinations.iter().eq(self.source.members.keys())
        {
            return None;
        }
        Some(roles)
    }

    /// Check bounded binary headers and source identity on the held stages.
    /// This remains a point-in-time observation, not publication authority.
    pub(super) fn verify_staged_semantics(
        &self,
        slots: &mut BTreeMap<String, super::package_stage::StageSlot>,
    ) -> Option<()> {
        self.verify_staged_semantics_for(slots, observed_host_architecture()?)
    }

    fn verify_staged_semantics_for(
        &self,
        slots: &mut BTreeMap<String, super::package_stage::StageSlot>,
        architecture: super::package::ManifestArchitecture,
    ) -> Option<()> {
        if self.payload.architecture != architecture || self.stage_roles()?.iter().ne(slots.keys())
        {
            return None;
        }
        for role in ["ramshared", "ramsharedd"] {
            let declaration = self
                .payload
                .contents
                .iter()
                .find(|member| member.role == role)?;
            let mode = u32::from_str_radix(&declaration.mode, 8).ok()?;
            let bytes = self.staged_prefix(slots, role, 64, false)?;
            if mode & 0o111 == 0 || !binary_header_matches(&bytes, architecture) {
                return None;
            }
        }
        let mut line = |role: &str| {
            let declaration = self
                .payload
                .contents
                .iter()
                .find(|member| member.role == role)?;
            if super::package_member::bundle_destination(declaration).ok()?? != role {
                return None;
            }
            let bytes = self.staged_prefix(
                slots,
                role,
                crate::release_identity::MAX_IDENTITY_LINE_BYTES,
                true,
            )?;
            crate::release_identity::parse_identity_line(&bytes)
        };
        line("RELEASE_VERSION")?;
        let expected_tree = match self.payload.source.tree_state {
            super::package::ManifestTreeState::Clean => "clean",
            super::package::ManifestTreeState::Dirty => "dirty",
        };
        if line("SOURCE_COMMIT").as_deref() != Some(self.payload.source.commit.as_str())
            || line("SOURCE_BRANCH").as_deref() != Some(self.payload.source.branch.as_str())
            || line("SOURCE_TREE_STATE").as_deref() != Some(expected_tree)
        {
            return None;
        }
        Some(())
    }

    fn staged_prefix(
        &self,
        slots: &mut BTreeMap<String, super::package_stage::StageSlot>,
        role: &str,
        limit: usize,
        whole: bool,
    ) -> Option<Vec<u8>> {
        let expected = self.member_digest(role)?;
        let size = self.member_size(role)?;
        let slot = slots.get_mut(role)?;
        slot.verify_bytes(expected, size)?;
        let bytes = slot.read_prefix(limit, whole)?;
        slot.verify_bytes(expected, size)?;
        Some(bytes)
    }

    /// Capture the entire declared bundle within the caller's memory budget.
    #[cfg(test)]
    pub(super) fn verify_all(&self, budget: usize) -> Option<VerifiedBundle> {
        self.verify_all_for(budget, observed_host_architecture()?)
    }

    #[cfg(test)]
    fn verify_all_for(
        &self,
        budget: usize,
        architecture: super::package::ManifestArchitecture,
    ) -> Option<VerifiedBundle> {
        if self.payload.architecture != architecture {
            return None;
        }
        self.stage_roles()?;
        let mut remaining = budget;
        let mut members = BTreeMap::new();
        for declaration in &self.payload.contents {
            let Some(destination) = super::package_member::bundle_destination(declaration).ok()?
            else {
                continue;
            };
            let bytes = if destination == "SHA256SUMS" {
                if self.source.manifest.len() > remaining {
                    return None;
                }
                let mut copy = Vec::new();
                copy.try_reserve_exact(self.source.manifest.len()).ok()?;
                if copy.capacity() > remaining {
                    return None;
                }
                copy.extend_from_slice(&self.source.manifest);
                copy
            } else {
                self.member(&declaration.role, remaining.min(MEMBER_LIMIT))?
            };
            remaining = remaining.checked_sub(bytes.capacity())?;
            if members
                .insert(
                    declaration.role.clone(),
                    VerifiedMember {
                        destination,
                        mode: declaration.mode.clone(),
                        bytes,
                    },
                )
                .is_some()
            {
                return None;
            }
        }
        for role in ["ramshared", "ramsharedd"] {
            let binary = members.get(role)?;
            let mode = u32::from_str_radix(&binary.mode, 8).ok()?;
            if mode & 0o111 == 0 || !binary_header_matches(&binary.bytes, architecture) {
                return None;
            }
        }
        let line = |role: &str| {
            let member = members.get(role)?;
            if member.destination != role {
                return None;
            }
            crate::release_identity::parse_identity_line(&member.bytes)
        };
        let expected_tree = match self.payload.source.tree_state {
            super::package::ManifestTreeState::Clean => "clean",
            super::package::ManifestTreeState::Dirty => "dirty",
        };
        line("RELEASE_VERSION")?;
        if line("SOURCE_COMMIT").as_deref() != Some(self.payload.source.commit.as_str())
            || line("SOURCE_BRANCH").as_deref() != Some(self.payload.source.branch.as_str())
            || line("SOURCE_TREE_STATE").as_deref() != Some(expected_tree)
        {
            return None;
        }
        Some(VerifiedBundle {
            manifest_sha256: self.source.manifest_sha256.clone(),
            members,
        })
    }
}

fn safe_relative(path: &str) -> bool {
    !path.is_empty()
        && !path.chars().any(char::is_control)
        && path.split('/').all(|part| !matches!(part, "" | "." | ".."))
}

/// Check the fixed ELF header only; this is not full executable validation.
fn binary_header_matches(bytes: &[u8], architecture: super::package::ManifestArchitecture) -> bool {
    use super::package::ManifestArchitecture::{Amd64, Arm64};
    let Some(header) = bytes.get(..64) else {
        return false;
    };
    let machine = match architecture {
        Amd64 => 62u16,
        Arm64 => 183u16,
    };
    header.get(..7) == Some(&b"\x7fELF\x02\x01\x01"[..])
        && matches!(u16::from_le_bytes([header[16], header[17]]), 2 | 3)
        && u16::from_le_bytes([header[18], header[19]]) == machine
        && u32::from_le_bytes([header[20], header[21], header[22], header[23]]) == 1
        && u16::from_le_bytes([header[52], header[53]]) == 64
}

fn digest(bytes: &[u8]) -> Option<String> {
    let mut input = bytes;
    crate::installed_image::sha256_reader(&mut input)
}

/// Bound allocation before reading; refuse a changing file or allocation failure.
fn read_capped(mut file: File, limit: usize) -> Option<Vec<u8>> {
    let size = usize::try_from(file.metadata().ok()?.len()).ok()?;
    if size > limit {
        return None;
    }
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(size).ok()?;
    if bytes.capacity() > limit {
        return None;
    }
    bytes.resize(size, 0);
    file.read_exact(&mut bytes).ok()?;
    let mut extra = [0u8; 1];
    if file.read(&mut extra).ok()? != 0 {
        return None;
    }
    Some(bytes)
}

fn architecture_from_machine(machine: &str) -> Option<super::package::ManifestArchitecture> {
    use super::package::ManifestArchitecture::{Amd64, Arm64};
    match machine {
        "x86_64" => Some(Amd64),
        "aarch64" => Some(Arm64),
        _ => None,
    }
}

fn observed_host_architecture() -> Option<super::package::ManifestArchitecture> {
    let name = rustix::system::uname();
    architecture_from_machine(name.machine().to_str().ok()?)
}

impl ApprovedBundle {
    pub(super) fn open_verified(
        path: &Path,
        signature: &super::package_trust::VerifiedSignature,
    ) -> Option<Self> {
        signature.revalidate()?;
        Self::open(path, signature.manifest_sha256())
    }
    /// Copy one approved member in bounded chunks. Any failed stage is retained.
    pub(super) fn copy_member_to(
        &self,
        path: &str,
        slot: &mut super::package_stage::StageSlot,
        limit: u64,
    ) -> Result<u64, super::package_stage::StageError> {
        use super::package_stage::StageError;
        let expected = self.members.get(path).ok_or(StageError::Refused)?;
        super::package_stage::copy_verified(
            self.open_member(path).ok_or(StageError::Refused)?,
            expected,
            slot,
            limit.min(MEMBER_LIMIT as u64),
        )
    }

    pub(super) fn open(path: &Path, approved_manifest_sha256: &str) -> Option<Self> {
        if !path.is_absolute() || !super::package::is_lowercase_sha256(approved_manifest_sha256) {
            return None;
        }
        let root = open_directory_no_follow(path, false).ok()??;
        let bytes = read_capped(open_regular_at(&root, "SHA256SUMS")?, MANIFEST_LIMIT)?;
        if digest(&bytes).as_deref() != Some(approved_manifest_sha256) {
            return None;
        }
        let text = std::str::from_utf8(&bytes).ok()?;
        let mut members = BTreeMap::new();
        for line in text.lines() {
            let (hash, name) = line.split_once("  ./")?;
            if !super::package::is_lowercase_sha256(hash)
                || !safe_relative(name)
                || members.insert(name.to_owned(), hash.to_owned()).is_some()
            {
                return None;
            }
        }
        if members.is_empty() || !text.ends_with('\n') {
            return None;
        }
        Some(Self {
            root,
            members,
            manifest: bytes,
            manifest_sha256: approved_manifest_sha256.to_owned(),
        })
    }

    /// Return the exact verified bytes, not a pathname that can be replaced.
    pub(super) fn member(&self, path: &str, limit: usize) -> Option<Vec<u8>> {
        if !safe_relative(path) || limit > MEMBER_LIMIT {
            return None;
        }
        let expected = self.members.get(path)?;
        let file = self.open_member(path)?;
        let bytes = read_capped(file, limit)?;
        (digest(&bytes).as_deref() == Some(expected.as_str())).then_some(bytes)
    }

    fn open_member(&self, path: &str) -> Option<File> {
        if !safe_relative(path) {
            return None;
        }
        let mut parts = path.split('/').peekable();
        let mut directory = self.root.try_clone().ok()?;
        while let Some(part) = parts.next() {
            if parts.peek().is_none() {
                return open_regular_at(&directory, part);
            }
            directory = File::from(
                openat(
                    &directory,
                    part,
                    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                    Mode::empty(),
                )
                .ok()?,
            );
        }
        None
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn approved_member_streams_to_exclusive_slot_and_preserves_failed_evidence() {
        use crate::customer::package_stage::{StageError, StageSlot};
        let root =
            std::env::temp_dir().join(format!("ramshared-stream-source-{}", std::process::id()));
        std::fs::create_dir_all(root.join("stage")).unwrap();
        let content = vec![42u8; 200_000];
        std::fs::write(root.join("input"), &content).unwrap();
        let manifest = format!("{}  ./input\n", digest(&content).unwrap());
        std::fs::write(root.join("SHA256SUMS"), &manifest).unwrap();
        let bundle = ApprovedBundle::open(&root, &digest(manifest.as_bytes()).unwrap()).unwrap();
        let mut slot = StageSlot::create(&root.join("stage"), "one").unwrap();
        assert_eq!(
            bundle.copy_member_to("input", &mut slot, content.len() as u64),
            Ok(content.len() as u64)
        );
        assert_eq!(std::fs::read(root.join("stage/one")).unwrap(), content);
        let mut limited = StageSlot::create(&root.join("stage"), "limited").unwrap();
        assert_eq!(
            bundle.copy_member_to("input", &mut limited, 1),
            Err(StageError::Refused)
        );
        assert!(root.join("stage/limited").exists());
        let mut replaced = StageSlot::create(&root.join("stage"), "replaced").unwrap();
        std::fs::rename(root.join("stage/replaced"), root.join("stage/original")).unwrap();
        std::fs::write(root.join("stage/replaced"), b"foreign").unwrap();
        assert_eq!(
            bundle.copy_member_to("input", &mut replaced, content.len() as u64),
            Err(StageError::RecoveryRequired)
        );
        assert_eq!(
            std::fs::read(root.join("stage/replaced")).unwrap(),
            b"foreign"
        );
        let mut changed = StageSlot::create(&root.join("stage"), "changed").unwrap();
        std::fs::write(root.join("input"), b"changed").unwrap();
        assert_eq!(
            bundle.copy_member_to("input", &mut changed, 100),
            Err(StageError::RecoveryRequired)
        );
        assert!(root.join("stage/changed").exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn bounded_capture_refuses_oversized_sparse_files_without_reading_them() {
        let root =
            std::env::temp_dir().join(format!("ramshared-sparse-source-{}", std::process::id()));
        std::fs::create_dir(&root).unwrap();
        let file = root.join("member");
        std::fs::File::create(&file)
            .unwrap()
            .set_len(1024 * 1024 * 1024)
            .unwrap();
        assert!(read_capped(std::fs::File::open(&file).unwrap(), 32).is_none());
        std::fs::write(&file, b"exact").unwrap();
        assert_eq!(
            read_capped(std::fs::File::open(&file).unwrap(), 5),
            Some(b"exact".to_vec())
        );
        assert!(read_capped(std::fs::File::open(&file).unwrap(), 4).is_none());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn host_architecture_refuses_unknown_machine_names() {
        use crate::customer::package::ManifestArchitecture::{Amd64, Arm64};
        assert_eq!(architecture_from_machine("x86_64"), Some(Amd64));
        assert_eq!(architecture_from_machine("aarch64"), Some(Arm64));
        assert_eq!(architecture_from_machine("amd64"), None);
        assert_eq!(architecture_from_machine("x86_64\n"), None);
        assert_eq!(architecture_from_machine(""), None);
        let actual = rustix::system::uname();
        assert_eq!(
            observed_host_architecture(),
            actual
                .machine()
                .to_str()
                .ok()
                .and_then(architecture_from_machine)
        );
    }

    fn fixture_elf(machine: u16) -> Vec<u8> {
        let mut header = vec![0u8; 64];
        header[..4].copy_from_slice(b"\x7fELF");
        header[4..7].copy_from_slice(&[2, 1, 1]);
        header[16..18].copy_from_slice(&3u16.to_le_bytes());
        header[18..20].copy_from_slice(&machine.to_le_bytes());
        header[20..24].copy_from_slice(&1u32.to_le_bytes());
        header[52..54].copy_from_slice(&64u16.to_le_bytes());
        header
    }

    #[test]
    fn binary_header_must_match_declared_linux_architecture() {
        use crate::customer::package::ManifestArchitecture::{Amd64, Arm64};
        let mut header = fixture_elf(62);
        assert!(binary_header_matches(&header, Amd64));
        assert!(!binary_header_matches(&header, Arm64));
        assert!(!binary_header_matches(&header[..20], Amd64));
        header[5] = 2;
        assert!(!binary_header_matches(&header, Amd64));
        header[5] = 1;
        header[18..20].copy_from_slice(&183u16.to_le_bytes());
        assert!(binary_header_matches(&header, Arm64));
        header[16..18].copy_from_slice(&1u16.to_le_bytes());
        assert!(!binary_header_matches(&header, Arm64));
        #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
        {
            use std::io::Read;
            let mut actual = [0u8; 64];
            std::fs::File::open("/proc/self/exe")
                .unwrap()
                .read_exact(&mut actual)
                .unwrap();
            let target = if cfg!(target_arch = "x86_64") {
                Amd64
            } else {
                Arm64
            };
            assert!(binary_header_matches(&actual, target));
        }
    }

    #[test]
    #[ignore = "subprocess fixture invoked by complete_bundle_requires_exact_inventory_and_retains_verified_bytes"]
    fn publication_kill_child() {
        use crate::customer::transition::{
            OperationOwner, TransitionPaths, acquire_transition, read_pending, recovery_refusal,
        };
        use std::io::{Read, Write};
        use std::os::unix::fs::MetadataExt;
        let root =
            std::path::PathBuf::from(std::env::var_os("RAMSHARED_PUBLICATION_CHILD_DIR").unwrap());
        let paths = TransitionPaths::under(&root.join("stream-transaction"));
        let guard = acquire_transition(OperationOwner::Package, &paths).unwrap();
        let pending = read_pending(&paths).unwrap().unwrap();
        let bundle = DeclaredBundle::open(&root, &pending.new_hash).unwrap();
        let release = root.join("publication-root");
        let meta = std::fs::metadata(&release).unwrap();
        let result = crate::customer::package_publish::publish_with_hook(
            &guard,
            &pending,
            &bundle,
            &release,
            (meta.dev(), meta.ino()),
            "ramshared",
            || {
                println!("RAMSHARED_PUBLICATION_READY");
                std::io::stdout().flush().unwrap();
                let mut byte = [0u8; 1];
                let _ = std::io::stdin().read(&mut byte);
                Err(recovery_refusal("test interruption"))
            },
        );
        assert!(result.is_err());
    }

    #[test]
    #[ignore = "subprocess fixture invoked by complete_bundle_requires_exact_inventory_and_retains_verified_bytes"]
    fn tree_kill_child() {
        use crate::customer::transition::{
            OperationOwner, TransitionPaths, acquire_transition, read_pending, recovery_refusal,
        };
        use std::io::{Read, Write};
        use std::os::unix::fs::MetadataExt;
        let root = std::path::PathBuf::from(std::env::var_os("RAMSHARED_TREE_CHILD_DIR").unwrap());
        let paths = TransitionPaths::under(&root.join("tree-kill-transaction"));
        let guard = acquire_transition(OperationOwner::Package, &paths).unwrap();
        let pending = read_pending(&paths).unwrap().unwrap();
        let bundle = DeclaredBundle::open(&root, &pending.new_hash).unwrap();
        let parent = root.join("tree-test-parent");
        let meta = std::fs::metadata(&parent).unwrap();
        let result = crate::customer::package_tree::continue_tree(
            &guard,
            &pending,
            &bundle,
            (&parent, (meta.dev(), meta.ino()), 0),
            "killed",
            false,
            &mut |_| {
                println!("RAMSHARED_TREE_READY");
                std::io::stdout().flush().unwrap();
                let mut byte = [0u8; 1];
                let _ = std::io::stdin().read(&mut byte);
                Err(recovery_refusal("test interruption"))
            },
        );
        assert!(result.is_err());
    }

    include!("package_source/tests/fixture_setup.rs");
    include!("package_source/tests/tree_scenarios.rs");
    include!("package_source/tests/reconciliation_scenarios.rs");
    include!("package_source/tests/publication_boundary_scenarios.rs");
    include!("package_source/tests/tree_recovery_scenarios.rs");
    include!("package_source/tests/publication_receipt_scenarios.rs");
    include!("package_source/tests/stage_recovery_scenarios.rs");
    include!("package_source/tests/inventory_publication_scenarios.rs");
    include!("package_source/tests/source_admission_scenarios.rs");
    include!("package_source/tests/package_prepare_scenarios.rs");
    include!("package_source/tests/preparation_recovery_scenarios.rs");
    include!("package_source/tests/final_admission_scenarios.rs");
    include!("package_source/tests/stage_setup_scenarios.rs");
    include!("package_source/tests/staged_semantics_scenarios.rs");
    include!("package_source/tests/complete_bundle.rs");

    #[test]
    fn declared_bundle_binds_outer_and_inner_manifests_to_member_bytes() {
        use crate::customer::package::{
            ManifestFormat, ManifestKind, ManifestMember, tests::pinned_manifest,
        };
        let root =
            std::env::temp_dir().join(format!("ramshared-declared-source-{}", std::process::id()));
        std::fs::create_dir_all(root.join("scripts/safety")).unwrap();
        let mut payload = pinned_manifest();
        payload.contents.push(ManifestMember {
            role: "RELEASE_VERSION".into(),
            kind: ManifestKind::Generated,
            mode: "0644".into(),
            source: "RELEASE_VERSION".into(),
            formats: vec![ManifestFormat::Bundle],
            sha256: None,
            reason: Some("Generated for bundle".into()),
        });
        let script = payload
            .contents
            .iter_mut()
            .find(|member| member.role == "origin.conf.example")
            .unwrap();
        assert_eq!(script.kind, ManifestKind::ConfigExample);
        script.sha256 = Some(digest(b"approved").unwrap());
        let document = serde_json::to_vec(&payload).unwrap();
        std::fs::write(root.join("release-manifest.json"), &document).unwrap();
        std::fs::write(root.join("RELEASE_VERSION"), b"v1\n").unwrap();
        std::fs::write(root.join("scripts/safety/origin.conf.example"), b"approved").unwrap();
        let checksums = format!(
            "{}  ./release-manifest.json\n{}  ./scripts/safety/origin.conf.example\n{}  ./RELEASE_VERSION\n",
            digest(&document).unwrap(),
            digest(b"approved").unwrap(),
            digest(b"v1\n").unwrap()
        );
        std::fs::write(root.join("SHA256SUMS"), &checksums).unwrap();
        let bundle = DeclaredBundle::open(&root, &digest(checksums.as_bytes()).unwrap()).unwrap();
        assert_eq!(
            bundle.member("origin.conf.example", 8),
            Some(b"approved".to_vec())
        );
        std::fs::create_dir(root.join("stage")).unwrap();
        let mut slot =
            crate::customer::package_stage::StageSlot::create(&root.join("stage"), "first")
                .unwrap();
        assert_eq!(
            bundle.stage_member("origin.conf.example", &mut slot, 8),
            Ok(8)
        );
        assert_eq!(
            std::fs::read(root.join("stage/first")).unwrap(),
            b"approved"
        );
        use crate::customer::package_stage_journal::{
            StagePhase, StageRequest, read_stage_journal, resume_staged_member,
            stage_declared_member,
        };
        use crate::customer::transition::{
            OperationOwner, PackageAction, PackagePending, TransitionPaths, acquire_transition,
            enroll_pending,
        };
        let transaction_root = root.join("journal-fixture");
        let paths = TransitionPaths::under(&transaction_root);
        let guard = acquire_transition(OperationOwner::Package, &paths).unwrap();
        let pending = PackagePending {
            target_release: None,
            action: PackageAction::Upgrade,
            old_hash: "a".repeat(64),
            new_hash: digest(checksums.as_bytes()).unwrap(),
            issued_unix: 1,
        };
        let staging = root.join("stage");
        let request = |name| StageRequest {
            pending: &pending,
            bundle: &bundle,
            role: "origin.conf.example",
            parent: &staging,
            name,
            limit: 8,
        };
        assert!(stage_declared_member(&guard, request("journaled")).is_err());
        assert!(resume_staged_member(&guard, &pending, &bundle).is_err());
        assert!(!root.join("stage/journaled").exists());
        enroll_pending(&guard, &pending).unwrap();
        assert!(resume_staged_member(&guard, &pending, &bundle).is_err());
        let staged = stage_declared_member(&guard, request("journaled")).unwrap();
        assert!(staged.revalidate().is_some());
        let journal = read_stage_journal(&paths).unwrap().unwrap();
        assert_eq!(journal.role, "origin.conf.example");
        assert_eq!(journal.phase, StagePhase::Verified);
        assert!(journal.inode.is_some());
        let journal_path = paths.transactions_root().join("package-stage.json");
        assert_eq!(
            std::fs::read_to_string(&journal_path)
                .unwrap()
                .lines()
                .count(),
            3
        );
        drop(staged);
        drop(guard);
        let guard = acquire_transition(OperationOwner::Package, &paths).unwrap();
        let mut mismatched = pending.clone();
        mismatched.new_hash = "c".repeat(64);
        assert!(resume_staged_member(&guard, &mismatched, &bundle).is_err());
        let resumed = resume_staged_member(&guard, &pending, &bundle).unwrap();
        assert!(resumed.revalidate().is_some());
        drop(resumed);
        let complete = std::fs::read_to_string(&journal_path).unwrap();
        let opened = format!(
            "{}\n",
            complete.lines().take(2).collect::<Vec<_>>().join("\n")
        );
        std::fs::write(&journal_path, opened).unwrap();
        assert_eq!(
            read_stage_journal(&paths).unwrap().unwrap().phase,
            StagePhase::Opened
        );
        let resumed = resume_staged_member(&guard, &pending, &bundle).unwrap();
        assert!(resumed.revalidate().is_some());
        drop(resumed);
        assert_eq!(
            read_stage_journal(&paths).unwrap().unwrap().phase,
            StagePhase::Verified
        );
        std::fs::rename(staging.join("journaled"), staging.join("original-stage")).unwrap();
        std::fs::write(staging.join("journaled"), b"approved").unwrap();
        assert!(resume_staged_member(&guard, &pending, &bundle).is_err());
        assert_eq!(
            std::fs::read(staging.join("journaled")).unwrap(),
            b"approved"
        );
        std::fs::remove_file(staging.join("journaled")).unwrap();
        std::fs::rename(staging.join("original-stage"), staging.join("journaled")).unwrap();
        let moved_stage = root.join("stage-moved");
        std::fs::rename(&staging, &moved_stage).unwrap();
        std::fs::create_dir(&staging).unwrap();
        std::fs::rename(moved_stage.join("journaled"), staging.join("journaled")).unwrap();
        assert!(resume_staged_member(&guard, &pending, &bundle).is_err());
        assert_eq!(
            std::fs::read(staging.join("journaled")).unwrap(),
            b"approved"
        );
        std::fs::rename(staging.join("journaled"), moved_stage.join("journaled")).unwrap();
        std::fs::remove_dir(&staging).unwrap();
        std::fs::rename(moved_stage, &staging).unwrap();
        std::fs::write(staging.join("journaled"), b"tampered").unwrap();
        assert!(resume_staged_member(&guard, &pending, &bundle).is_err());
        assert_eq!(
            std::fs::read(staging.join("journaled")).unwrap(),
            b"tampered"
        );
        assert!(stage_declared_member(&guard, request("second")).is_err());
        assert!(!root.join("stage/second").exists());
        assert!(crate::customer::transition::clear_pending(&guard, &pending).is_err());
        drop(guard);
        std::fs::remove_file(
            paths
                .transactions_root()
                .join(TransitionPaths::pending_file_name()),
        )
        .unwrap();
        assert!(acquire_transition(OperationOwner::Up, &paths).is_err());
        use std::io::Write;
        std::fs::OpenOptions::new()
            .append(true)
            .open(&journal_path)
            .unwrap()
            .write_all(b"{partial")
            .unwrap();
        assert!(read_stage_journal(&paths).is_err());
        assert!(acquire_transition(OperationOwner::Up, &paths).is_err());
        let failure_paths = TransitionPaths::under(&root.join("failed-transaction"));
        let failure_guard = acquire_transition(OperationOwner::Package, &failure_paths).unwrap();
        enroll_pending(&failure_guard, &pending).unwrap();
        let failure_request = || StageRequest {
            pending: &pending,
            bundle: &bundle,
            role: "origin.conf.example",
            parent: &staging,
            name: "limited",
            limit: 1,
        };
        let error = stage_declared_member(&failure_guard, failure_request())
            .err()
            .unwrap();
        assert_eq!(
            error.reason,
            crate::customer::CustomerReason::RecoveryRequired
        );
        assert_eq!(
            read_stage_journal(&failure_paths).unwrap().unwrap().phase,
            StagePhase::Opened
        );
        assert!(root.join("stage/limited").exists());
        assert!(resume_staged_member(&failure_guard, &pending, &bundle).is_err());
        assert!(root.join("stage/limited").exists());
        assert!(stage_declared_member(&failure_guard, failure_request()).is_err());
        let unknown_paths = TransitionPaths::under(&root.join("unknown-transaction"));
        let unknown_guard = acquire_transition(OperationOwner::Package, &unknown_paths).unwrap();
        enroll_pending(&unknown_guard, &pending).unwrap();
        let unknown_request = StageRequest {
            pending: &pending,
            bundle: &bundle,
            role: "unknown-role",
            parent: &staging,
            name: "unknown",
            limit: 8,
        };
        assert!(stage_declared_member(&unknown_guard, unknown_request).is_err());
        assert!(read_stage_journal(&unknown_paths).unwrap().is_none());
        assert!(resume_staged_member(&unknown_guard, &pending, &bundle).is_err());
        assert!(!staging.join("unknown").exists());
        let collision_paths = TransitionPaths::under(&root.join("collision-transaction"));
        let collision_guard =
            acquire_transition(OperationOwner::Package, &collision_paths).unwrap();
        enroll_pending(&collision_guard, &pending).unwrap();
        assert!(read_stage_journal(&collision_paths).unwrap().is_none());
        let invalid = StageRequest {
            pending: &pending,
            bundle: &bundle,
            role: "origin.conf.example",
            parent: &staging,
            name: "../escape",
            limit: 8,
        };
        assert!(stage_declared_member(&collision_guard, invalid).is_err());
        assert!(read_stage_journal(&collision_paths).unwrap().is_none());
        std::fs::write(staging.join("foreign"), b"foreign").unwrap();
        let collision = StageRequest {
            pending: &pending,
            bundle: &bundle,
            role: "origin.conf.example",
            parent: &staging,
            name: "foreign",
            limit: 8,
        };
        assert!(stage_declared_member(&collision_guard, collision).is_err());
        assert_eq!(
            read_stage_journal(&collision_paths).unwrap().unwrap().phase,
            StagePhase::Intent
        );
        assert!(resume_staged_member(&collision_guard, &pending, &bundle).is_err());
        assert_eq!(std::fs::read(staging.join("foreign")).unwrap(), b"foreign");
        assert_eq!(bundle.member("RELEASE_VERSION", 10), Some(b"v1\n".to_vec()));
        std::fs::write(root.join("RELEASE_VERSION"), b"v2\n").unwrap();
        assert!(bundle.member("RELEASE_VERSION", 10).is_none());
        assert!(bundle.member("missing", 100).is_none());
        let mut disagreeing = payload.clone();
        disagreeing
            .contents
            .iter_mut()
            .find(|member| member.role == "origin.conf.example")
            .unwrap()
            .sha256 = Some(digest(b"other").unwrap());
        let different_document = serde_json::to_vec(&disagreeing).unwrap();
        std::fs::write(root.join("release-manifest.json"), &different_document).unwrap();
        let different_checksums = format!(
            "{}  ./release-manifest.json\n{}  ./scripts/safety/origin.conf.example\n",
            digest(&different_document).unwrap(),
            digest(b"approved").unwrap()
        );
        std::fs::write(root.join("SHA256SUMS"), &different_checksums).unwrap();
        let disagreeing_bundle =
            DeclaredBundle::open(&root, &digest(different_checksums.as_bytes()).unwrap()).unwrap();
        assert!(
            disagreeing_bundle
                .member("origin.conf.example", 100)
                .is_none()
        );
        let mut refused =
            crate::customer::package_stage::StageSlot::create(&root.join("stage"), "refused")
                .unwrap();
        assert!(
            disagreeing_bundle
                .stage_member("origin.conf.example", &mut refused, 100)
                .is_err()
        );
        assert!(
            std::fs::read(root.join("stage/refused"))
                .unwrap()
                .is_empty()
        );
        std::fs::write(root.join("scripts/safety/origin.conf.example"), b"modified").unwrap();
        assert!(bundle.member("origin.conf.example", 100).is_none());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn approved_bundle_pins_manifest_and_verifies_member_bytes() {
        let root =
            std::env::temp_dir().join(format!("ramshared-approved-source-{}", std::process::id()));
        std::fs::create_dir_all(root.join("scripts/safety")).unwrap();
        let file = root.join("scripts/safety/installer.sh");
        std::fs::write(&file, b"approved").unwrap();
        let manifest = format!(
            "{}  ./scripts/safety/installer.sh\n",
            digest(b"approved").unwrap()
        );
        std::fs::write(root.join("SHA256SUMS"), &manifest).unwrap();
        let approved = digest(manifest.as_bytes()).unwrap();
        let bundle = ApprovedBundle::open(&root, &approved).unwrap();
        assert_eq!(
            bundle.member("scripts/safety/installer.sh", 8),
            Some(b"approved".to_vec())
        );
        assert!(bundle.member("scripts/safety/installer.sh", 7).is_none());
        assert!(bundle.member("../installer.sh", 20).is_none());
        assert!(bundle.member("scripts/safety/unknown", 20).is_none());
        std::fs::write(root.join("SHA256SUMS"), b"replaced\n").unwrap();
        assert_eq!(
            bundle.member("scripts/safety/installer.sh", 8),
            Some(b"approved".to_vec())
        );
        assert!(ApprovedBundle::open(&root, &approved).is_none());
        std::fs::write(&file, b"modified").unwrap();
        assert!(bundle.member("scripts/safety/installer.sh", 20).is_none());
        std::fs::remove_file(&file).unwrap();
        symlink("/etc/passwd", &file).unwrap();
        assert!(bundle.member("scripts/safety/installer.sh", 20).is_none());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn approved_bundle_reads_from_pinned_root_after_path_replacement() {
        let root =
            std::env::temp_dir().join(format!("ramshared-moved-source-{}", std::process::id()));
        let moved = root.with_extension("moved");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("member"), b"trusted").unwrap();
        let manifest = format!("{}  ./member\n", digest(b"trusted").unwrap());
        std::fs::write(root.join("SHA256SUMS"), &manifest).unwrap();
        let bundle = ApprovedBundle::open(&root, &digest(manifest.as_bytes()).unwrap()).unwrap();
        std::fs::rename(&root, &moved).unwrap();
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("member"), b"foreign").unwrap();
        assert_eq!(bundle.member("member", 20), Some(b"trusted".to_vec()));
        std::fs::remove_dir_all(root).unwrap();
        std::fs::remove_dir_all(moved).unwrap();
    }

    #[test]
    fn approved_bundle_rejects_ambiguous_manifests_and_directory_redirects() {
        let root =
            std::env::temp_dir().join(format!("ramshared-invalid-source-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let hash = digest(b"x").unwrap();
        for text in [
            format!("{hash}  ./../escape\n"),
            format!("{hash}  ./x\n{hash}  ./x\n"),
            format!("{hash}  ./x"),
            String::new(),
        ] {
            std::fs::write(root.join("SHA256SUMS"), &text).unwrap();
            assert!(ApprovedBundle::open(&root, &digest(text.as_bytes()).unwrap()).is_none());
        }
        let text = format!("{hash}  ./link/file\n");
        std::fs::write(root.join("SHA256SUMS"), &text).unwrap();
        symlink("/tmp", root.join("link")).unwrap();
        let bundle = ApprovedBundle::open(&root, &digest(text.as_bytes()).unwrap()).unwrap();
        assert!(bundle.member("link/file", 10).is_none());
        assert!(ApprovedBundle::open(&root, "invalid").is_none());
        assert!(
            ApprovedBundle::open(Path::new("relative"), &digest(text.as_bytes()).unwrap())
                .is_none()
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
