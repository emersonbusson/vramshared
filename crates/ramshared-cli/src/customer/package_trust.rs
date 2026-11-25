// SPDX-License-Identifier: MIT
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/customer/package_trust.rs
//! Descriptor-held trust inputs for future publisher verification.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::Duration;

use rustix::fs::{
    MemfdFlags, Mode, OFlags, SealFlags, fcntl_add_seals, fcntl_get_seals, memfd_create, open,
    openat,
};

// Bound descriptor use for adversarial input paths, independently of the host layout.
const MAX_TRUST_PATH_DEPTH: usize = 128;
// Trust metadata uses the same 1 MiB ceiling as the existing release verifier.
const MAX_TRUST_SNAPSHOT_BYTES: u64 = 1024 * 1024;

/// A manifest verified against a separately selected, protected keyring.
pub(super) struct VerifiedSignature {
    manifest_sha256: String,
    custody: SignatureCustody,
}

enum SignatureCustody {
    Held(Box<Mutex<SignatureInputs>>),
    #[cfg(test)]
    Fixture(Option<Vec<u8>>),
}

struct SignatureInputs {
    policy: Option<TrustedFile>,
    keyring: TrustedFile,
    verification: VerificationSnapshots,
}

struct VerificationSnapshots {
    verifier: File,
    keyring: File,
    signature: File,
    manifest: File,
}

impl VerifiedSignature {
    pub(super) fn manifest_sha256(&self) -> &str {
        &self.manifest_sha256
    }

    /// Identify retained verification inputs; this is not an authorization.
    pub(super) fn trust_reference(&self) -> Option<String> {
        use sha2::{Digest, Sha256};
        match &self.custody {
            SignatureCustody::Held(inputs) => {
                let mut inputs = inputs.try_lock().ok()?;
                if let Some(policy) = inputs.policy.as_mut() {
                    policy.revalidate()?;
                }
                inputs.keyring.revalidate()?;
                let seals =
                    SealFlags::WRITE | SealFlags::SHRINK | SealFlags::GROW | SealFlags::SEAL;
                for file in [
                    &inputs.verification.verifier,
                    &inputs.verification.signature,
                ] {
                    if !fcntl_get_seals(file).ok()?.contains(seals) {
                        return None;
                    }
                }
                let verifier_len = inputs.verification.verifier.metadata().ok()?.len();
                let signature_len = inputs.verification.signature.metadata().ok()?.len();
                if !(1..=MAX_TRUST_SNAPSHOT_BYTES).contains(&verifier_len)
                    || !(1..=MAX_TRUST_SNAPSHOT_BYTES).contains(&signature_len)
                {
                    return None;
                }
                let verifier = file_digest(&mut inputs.verification.verifier, verifier_len)?;
                let signature = file_digest(&mut inputs.verification.signature, signature_len)?;
                let mut digest = Sha256::new();
                if let Some(policy) = inputs.policy.as_ref() {
                    digest.update(b"ramshared-release-trust-reference-v1\0");
                    for value in [
                        self.manifest_sha256.as_bytes(),
                        policy.path.as_os_str().as_bytes(),
                        policy.digest.as_bytes(),
                        inputs.keyring.path.as_os_str().as_bytes(),
                        inputs.keyring.digest.as_bytes(),
                        verifier.as_bytes(),
                        signature.as_bytes(),
                    ] {
                        digest.update((value.len() as u64).to_be_bytes());
                        digest.update(value);
                    }
                    for value in [
                        policy.identity.0,
                        policy.identity.1,
                        inputs.keyring.identity.0,
                        inputs.keyring.identity.1,
                    ] {
                        digest.update(value.to_be_bytes());
                    }
                } else {
                    digest.update(b"ramshared-release-direct-keyring-v1\0");
                    for value in [
                        self.manifest_sha256.as_bytes(),
                        inputs.keyring.path.as_os_str().as_bytes(),
                        inputs.keyring.digest.as_bytes(),
                        verifier.as_bytes(),
                        signature.as_bytes(),
                    ] {
                        digest.update((value.len() as u64).to_be_bytes());
                        digest.update(value);
                    }
                    for value in [inputs.keyring.identity.0, inputs.keyring.identity.1] {
                        digest.update(value.to_be_bytes());
                    }
                }
                inputs.keyring.revalidate()?;
                if let Some(policy) = inputs.policy.as_mut() {
                    policy.revalidate()?;
                }
                Some(
                    digest
                        .finalize()
                        .iter()
                        .map(|byte| format!("{byte:02x}"))
                        .collect(),
                )
            }
            #[cfg(test)]
            SignatureCustody::Fixture(_) => Some(crate::resource_config::profile_digest(&format!(
                "ramshared-test-trust:{}",
                self.manifest_sha256
            ))),
        }
    }

    pub(super) fn revalidate(&self) -> Option<()> {
        match &self.custody {
            SignatureCustody::Held(inputs) => {
                let mut inputs = inputs.try_lock().ok()?;
                if let Some(policy) = inputs.policy.as_mut() {
                    policy.revalidate()?;
                }
                inputs.keyring.revalidate()?;
                let snapshot = &inputs.verification;
                verify_sealed_gpgv(
                    &snapshot.verifier,
                    &snapshot.keyring,
                    &snapshot.signature,
                    &snapshot.manifest,
                )?;
                inputs.keyring.revalidate()?;
                if let Some(policy) = inputs.policy.as_mut() {
                    policy.revalidate()?;
                }
                Some(())
            }
            #[cfg(test)]
            SignatureCustody::Fixture(_) => Some(()),
        }
    }

    /// Return the sealed manifest that this proof authenticated.
    pub(super) fn verified_manifest(&self) -> Option<Vec<u8>> {
        use std::os::unix::fs::FileExt;
        self.revalidate()?;
        let bytes = match &self.custody {
            SignatureCustody::Held(inputs) => {
                let inputs = inputs.try_lock().ok()?;
                let manifest = &inputs.verification.manifest;
                let seals =
                    SealFlags::WRITE | SealFlags::SHRINK | SealFlags::GROW | SealFlags::SEAL;
                if !fcntl_get_seals(manifest).ok()?.contains(seals) {
                    return None;
                }
                let len = usize::try_from(manifest.metadata().ok()?.len()).ok()?;
                if len == 0 || u64::try_from(len).ok()? > MAX_TRUST_SNAPSHOT_BYTES {
                    return None;
                }
                let mut bytes = vec![0; len];
                manifest.read_exact_at(&mut bytes, 0).ok()?;
                bytes
            }
            #[cfg(test)]
            SignatureCustody::Fixture(bytes) => bytes.clone()?,
        };
        let digest = crate::installed_image::sha256_reader(&mut bytes.as_slice())?;
        (digest == self.manifest_sha256 && self.revalidate().is_some()).then_some(bytes)
    }

    #[cfg(test)]
    pub(super) fn fixture(manifest_sha256: String) -> Self {
        Self {
            manifest_sha256,
            custody: SignatureCustody::Fixture(None),
        }
    }

    #[cfg(test)]
    pub(super) fn fixture_with_manifest(bytes: Vec<u8>) -> Self {
        let manifest_sha256 =
            crate::installed_image::sha256_reader(&mut bytes.as_slice()).unwrap_or_default();
        Self {
            manifest_sha256,
            custody: SignatureCustody::Fixture(Some(bytes)),
        }
    }
}

/// Read the same protected policy used by the scheduled updater.
pub(super) fn verify_bundle(bundle: &Path) -> Option<VerifiedSignature> {
    verify_bundle_from_policy(bundle, Path::new("/etc/ramshared/auto-update.conf"), 0)
}

pub(super) fn verify_bundle_from_policy(
    bundle: &Path,
    policy: &Path,
    owner: u32,
) -> Option<VerifiedSignature> {
    verify_bundle_from_policy_with(bundle, policy, owner, || {})
}

pub(super) fn verify_bundle_with_keyring(
    bundle: &Path,
    keyring: &Path,
    owner: u32,
) -> Option<VerifiedSignature> {
    let (manifest_sha256, mut keyring, verification) =
        verify_bundle_for_owner(bundle, keyring, owner)?;
    keyring.revalidate()?;
    Some(VerifiedSignature {
        manifest_sha256,
        custody: SignatureCustody::Held(Box::new(Mutex::new(SignatureInputs {
            policy: None,
            keyring,
            verification,
        }))),
    })
}

fn verify_bundle_from_policy_with(
    bundle: &Path,
    policy: &Path,
    owner: u32,
    after_verify: impl FnOnce(),
) -> Option<VerifiedSignature> {
    if policy.starts_with(bundle) {
        return None;
    }
    let mut trusted = TrustedFile::open_for_owner(policy, MAX_TRUST_SNAPSHOT_BYTES, owner)?;
    let mut bytes = Vec::new();
    trusted.snapshot()?.read_to_end(&mut bytes).ok()?;
    let keyring = keyring_from_policy(&bytes)?;
    let (manifest_sha256, mut keyring, verification) =
        verify_bundle_for_owner(bundle, &keyring, owner)?;
    after_verify();
    trusted.revalidate()?;
    keyring.revalidate()?;
    Some(VerifiedSignature {
        manifest_sha256,
        custody: SignatureCustody::Held(Box::new(Mutex::new(SignatureInputs {
            policy: Some(trusted),
            keyring,
            verification,
        }))),
    })
}

fn keyring_from_policy(bytes: &[u8]) -> Option<PathBuf> {
    let text = std::str::from_utf8(bytes).ok()?;
    if text.chars().any(|ch| ch.is_control() && ch != '\n') {
        return None;
    }
    let mut keyring = None;
    for line in text.lines() {
        let Some((name, value)) = line.split_once('=') else {
            if line.trim() == "release_keyring" {
                return None;
            }
            continue;
        };
        if name.trim() != "release_keyring" {
            continue;
        }
        if name != "release_keyring"
            || keyring.is_some()
            || !value.starts_with('/')
            || value
                .as_bytes()
                .split(|byte| *byte == b'/')
                .skip(1)
                .any(|part| part.is_empty() || part == b"." || part == b"..")
        {
            return None;
        }
        keyring = Some(PathBuf::from(value));
    }
    keyring
}

fn verify_bundle_for_owner(
    bundle: &Path,
    keyring: &Path,
    owner: u32,
) -> Option<(String, TrustedFile, VerificationSnapshots)> {
    if !bundle.is_absolute() || !keyring.is_absolute() || keyring.starts_with(bundle) {
        return None;
    }
    // Match the system command directories used by the existing release verifier.
    let mut verifier = ["/usr/sbin/gpgv", "/usr/bin/gpgv", "/sbin/gpgv", "/bin/gpgv"]
        .iter()
        .filter_map(|path| TrustedFile::open(Path::new(path), MAX_TRUST_SNAPSHOT_BYTES))
        .find(|file| {
            file.file
                .metadata()
                .is_ok_and(|meta| meta.mode() & 0o111 != 0)
        })?;
    let mut key = TrustedFile::open_for_owner(keyring, MAX_TRUST_SNAPSHOT_BYTES, owner)?;
    let mut manifest =
        TrustedFile::open_for_owner(&bundle.join("SHA256SUMS"), MAX_TRUST_SNAPSHOT_BYTES, owner)?;
    let mut signature = TrustedFile::open_for_owner(
        &bundle.join("SHA256SUMS.asc"),
        MAX_TRUST_SNAPSHOT_BYTES,
        owner,
    )?;
    let executable_snapshot = verifier.snapshot()?;
    let key_snapshot = key.snapshot()?;
    let signature_snapshot = signature.snapshot()?;
    let manifest_snapshot = manifest.snapshot()?;
    verify_sealed_gpgv(
        &executable_snapshot,
        &key_snapshot,
        &signature_snapshot,
        &manifest_snapshot,
    )?;
    verifier.revalidate()?;
    key.revalidate()?;
    signature.revalidate()?;
    manifest.revalidate()?;
    Some((
        manifest.digest().to_owned(),
        key,
        VerificationSnapshots {
            verifier: executable_snapshot,
            keyring: key_snapshot,
            signature: signature_snapshot,
            manifest: manifest_snapshot,
        },
    ))
}

fn verify_sealed_gpgv(
    verifier: &File,
    keyring: &File,
    signature: &File,
    manifest: &File,
) -> Option<()> {
    verify_sealed_gpgv_with(
        verifier,
        keyring,
        signature,
        manifest,
        Duration::from_secs(10),
        |_| {},
    )
}

fn verify_sealed_gpgv_with(
    verifier: &File,
    keyring: &File,
    signature: &File,
    manifest: &File,
    timeout: Duration,
    on_spawn: impl FnOnce(u32),
) -> Option<()> {
    use rustix::fs::{PROC_SUPER_MAGIC, fstatfs};
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let proc = File::from(open("/proc", flags, Mode::empty()).ok()?);
    if fstatfs(&proc).ok()?.f_type != PROC_SUPER_MAGIC {
        return None;
    }
    let seals = SealFlags::WRITE | SealFlags::SHRINK | SealFlags::GROW | SealFlags::SEAL;
    for file in [verifier, keyring, signature, manifest] {
        if !fcntl_get_seals(file).ok()?.contains(seals)
            || !(1..=MAX_TRUST_SNAPSHOT_BYTES).contains(&file.metadata().ok()?.len())
        {
            return None;
        }
    }
    let path = |file: &File| format!("/proc/{}/fd/{}", std::process::id(), file.as_raw_fd());
    let mut command = Command::new(path(verifier));
    command.env_clear().stdin(Stdio::null()).args([
        "--quiet",
        "--status-fd",
        "1",
        "--keyring",
        &path(keyring),
        &path(signature),
        &path(manifest),
    ]);
    let output = ramshared_exec::run_capture_command(
        &mut command,
        "release signature verifier",
        timeout,
        16 * 1024,
        on_spawn,
    )
    .ok()?;
    let held = proc.metadata().ok()?;
    let named = std::fs::symlink_metadata("/proc").ok()?;
    (output.status.success()
        && valid_signature_status(&output.stdout)
        && named.is_dir()
        && (held.dev(), held.ino()) == (named.dev(), named.ino()))
        .then_some(())
}

/// gpgv may exit successfully for expired or revoked keys. Accept exactly
/// one unambiguous good signature and fail closed on every other status.
fn valid_signature_status(output: &[u8]) -> bool {
    let Ok(text) = std::str::from_utf8(output) else {
        return false;
    };
    if !text.ends_with('\n') {
        return false;
    }
    let mut phase = 0;
    for line in text.lines() {
        let Some(status) = line.strip_prefix("[GNUPG:] ") else {
            return false;
        };
        let kind = status.split_ascii_whitespace().next().unwrap_or("");
        match kind {
            "NEWSIG" if phase == 0 => phase = 1,
            "KEY_CONSIDERED" | "SIG_ID" if phase == 1 => (),
            "GOODSIG" if phase == 1 => phase = 2,
            "VALIDSIG" if phase == 2 => phase = 3,
            _ => return false,
        }
    }
    phase == 3
}

/// A root-controlled input held through verification. This is not a signature.
pub(super) struct TrustedFile {
    path: PathBuf,
    file: File,
    ancestors: Vec<File>,
    owner: u32,
    max_bytes: u64,
    identity: (u64, u64),
    digest: String,
}

impl TrustedFile {
    pub(super) fn open(path: &Path, max_bytes: u64) -> Option<Self> {
        Self::open_for_owner(path, max_bytes, 0)
    }

    fn open_for_owner(path: &Path, max_bytes: u64, owner: u32) -> Option<Self> {
        let (mut file, ancestors) = open_trusted(path, max_bytes, owner)?;
        let metadata = file.metadata().ok()?;
        let digest = file_digest(&mut file, metadata.len())?;
        if !valid_file(&file, max_bytes, owner) {
            return None;
        }
        Some(Self {
            path: path.to_path_buf(),
            file,
            ancestors,
            owner,
            max_bytes,
            identity: (metadata.dev(), metadata.ino()),
            digest,
        })
    }

    /// Verify the held bytes and the currently named object under the same policy.
    pub(super) fn revalidate(&mut self) -> Option<()> {
        self.revalidate_with_hook(|| {})
    }

    fn revalidate_with_hook(&mut self, after_content: impl FnOnce()) -> Option<()> {
        if !valid_file(&self.file, self.max_bytes, self.owner) {
            return None;
        }
        let (mut named, named_ancestors) = open_trusted(&self.path, self.max_bytes, self.owner)?;
        self.same_ancestors(&named_ancestors)?;
        let held_metadata = self.file.metadata().ok()?;
        let named_metadata = named.metadata().ok()?;
        if (held_metadata.dev(), held_metadata.ino()) != self.identity
            || (named_metadata.dev(), named_metadata.ino()) != self.identity
            || held_metadata.len() != named_metadata.len()
        {
            return None;
        }
        if file_digest(&mut self.file, held_metadata.len())? != self.digest
            || file_digest(&mut named, named_metadata.len())? != self.digest
            || !valid_file(&self.file, self.max_bytes, self.owner)
            || !valid_file(&named, self.max_bytes, self.owner)
        {
            return None;
        }
        after_content();
        let (final_named, final_ancestors) = open_trusted(&self.path, self.max_bytes, self.owner)?;
        self.same_ancestors(&final_ancestors)?;
        let final_metadata = final_named.metadata().ok()?;
        if (final_metadata.dev(), final_metadata.ino()) != self.identity
            || final_metadata.len() != held_metadata.len()
            || !valid_file(&self.file, self.max_bytes, self.owner)
        {
            return None;
        }
        Some(())
    }

    fn same_ancestors(&self, named_ancestors: &[File]) -> Option<()> {
        if self.ancestors.len() != named_ancestors.len() {
            return None;
        }
        for (index, (held, reopened)) in self.ancestors.iter().zip(named_ancestors).enumerate() {
            let protected_parent = index + 1 == self.ancestors.len();
            if !valid_directory(held, self.owner, protected_parent)
                || !valid_directory(reopened, self.owner, protected_parent)
            {
                return None;
            }
            let held_meta = held.metadata().ok()?;
            let named_meta = reopened.metadata().ok()?;
            if (held_meta.dev(), held_meta.ino()) != (named_meta.dev(), named_meta.ino()) {
                return None;
            }
        }
        Some(())
    }

    pub(super) fn digest(&self) -> &str {
        &self.digest
    }

    /// Capture immutable metadata bytes; authenticity still requires a signature.
    pub(super) fn snapshot(&mut self) -> Option<File> {
        let size = self.file.metadata().ok()?.len();
        if self.max_bytes > MAX_TRUST_SNAPSHOT_BYTES || size > MAX_TRUST_SNAPSHOT_BYTES {
            return None;
        }
        self.revalidate()?;
        let mut snapshot = File::from(
            memfd_create(
                "ramshared-trust",
                MemfdFlags::CLOEXEC | MemfdFlags::ALLOW_SEALING,
            )
            .ok()?,
        );
        self.file.seek(SeekFrom::Start(0)).ok()?;
        if std::io::copy(
            &mut (&mut self.file).take(size.checked_add(1)?),
            &mut snapshot,
        )
        .ok()?
            != size
        {
            return None;
        }
        let seals = SealFlags::WRITE | SealFlags::SHRINK | SealFlags::GROW | SealFlags::SEAL;
        fcntl_add_seals(&snapshot, seals).ok()?;
        if !fcntl_get_seals(&snapshot).ok()?.contains(seals)
            || file_digest(&mut snapshot, size)? != self.digest
        {
            return None;
        }
        self.revalidate()?;
        Some(snapshot)
    }
}

fn file_digest(file: &mut File, size: u64) -> Option<String> {
    file.seek(SeekFrom::Start(0)).ok()?;
    let hash = crate::installed_image::sha256_reader_with_limit(file, size)?;
    if file.metadata().ok()?.len() != size {
        return None;
    }
    file.seek(SeekFrom::Start(0)).ok()?;
    Some(hash)
}

fn valid_file(file: &File, max_bytes: u64, owner: u32) -> bool {
    file.metadata().is_ok_and(|meta| {
        meta.is_file()
            && meta.uid() == owner
            && meta.mode() & 0o022 == 0
            && meta.nlink() == 1
            && meta.len() > 0
            && meta.len() <= max_bytes
    })
}

fn valid_directory(file: &File, owner: u32, protected_parent: bool) -> bool {
    file.metadata().is_ok_and(|meta| {
        if !meta.is_dir() || (meta.uid() != 0 && meta.uid() != owner) {
            return false;
        }
        if meta.mode() & 0o022 == 0 {
            return true;
        }
        !protected_parent && meta.uid() == 0 && meta.mode() & 0o1000 != 0
    })
}

fn open_trusted(path: &Path, max_bytes: u64, owner: u32) -> Option<(File, Vec<File>)> {
    let raw = path.as_os_str().as_bytes();
    if !path.is_absolute()
        || raw.len() > ramshared_config::resource_profile::MAX_LINUX_RELATIVE_PATH_BYTES
        || raw.iter().any(u8::is_ascii_control)
        || raw
            .split(|byte| *byte == b'/')
            .skip(1)
            .any(|part| part.is_empty() || part == b"." || part == b"..")
    {
        return None;
    }
    let mut components = path.components();
    if components.next()? != Component::RootDir {
        return None;
    }
    let parts: Vec<_> = components
        .map(|component| match component {
            Component::Normal(value) => Some(value),
            _ => None,
        })
        .collect::<Option<_>>()?;
    if parts.is_empty() || parts.len() > MAX_TRUST_PATH_DEPTH {
        return None;
    }
    let directory_flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let root = File::from(open("/", directory_flags, Mode::empty()).ok()?);
    if !valid_directory(&root, owner, parts.len() == 1) {
        return None;
    }
    let mut ancestors = vec![root];
    for (index, component) in parts[..parts.len() - 1].iter().enumerate() {
        let directory = File::from(
            openat(
                ancestors.last()?,
                *component,
                directory_flags,
                Mode::empty(),
            )
            .ok()?,
        );
        if !valid_directory(&directory, owner, index == parts.len() - 2) {
            return None;
        }
        ancestors.push(directory);
    }
    let file = File::from(
        openat(
            ancestors.last()?,
            parts[parts.len() - 1],
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .ok()?,
    );
    valid_file(&file, max_bytes, owner).then_some((file, ancestors))
}

#[cfg(test)]
mod tests;
