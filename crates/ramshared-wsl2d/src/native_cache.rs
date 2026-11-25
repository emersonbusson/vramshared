// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/native_cache.rs
//! Immutable root-issued native cache generation and its serving lifetime.
use ramshared_config::cache_configuration::{CacheConfiguration, configuration_path, lease_path};
use ramshared_config::native_origin::NativeOriginManifest;
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

pub(super) mod worker;

#[derive(Debug)]
pub(super) struct NativeCacheSession {
    pub(super) lease: Option<File>,
    pub(super) policy: Option<CacheConfiguration>,
    pub(super) unavailable_reason: Option<String>,
    active: AtomicBool,
}
impl NativeCacheSession {
    pub(super) fn open(origin: &NativeOriginManifest) -> Result<Arc<Self>, String> {
        Self::open_with(lease_path(), 0, || load(origin))
    }
    fn open_with(
        path: &Path,
        owner: u32,
        read: impl FnOnce() -> Result<Option<CacheConfiguration>, String>,
    ) -> Result<Arc<Self>, String> {
        let lease =
            ramshared_exec::inherited_lease::acquire(path, owner).map_err(|e| e.to_string())?;
        let policy = read().and_then(|policy| {
            if let Some(policy) = &policy {
                policy.validate()?;
            }
            Ok(policy)
        });
        let (policy, unavailable_reason) = match policy {
            Ok(Some(policy)) => (Some(policy), None),
            Ok(None) => (None, Some("no approved native cache configuration".into())),
            Err(reason) => (
                None,
                Some(format!("native cache configuration unavailable: {reason}")),
            ),
        };
        Ok(Arc::new(Self {
            lease: Some(lease),
            policy,
            unavailable_reason,
            active: AtomicBool::new(true),
        }))
    }
    pub(super) fn is_active(&self) -> bool {
        self.active.load(Ordering::Acquire)
    }
    pub(super) fn unavailable(reason: String) -> Arc<Self> {
        Arc::new(Self {
            lease: None,
            policy: None,
            unavailable_reason: Some(reason),
            active: AtomicBool::new(false),
        })
    }
    pub(super) fn revoke(&self) {
        self.active.store(false, Ordering::Release);
    }
    pub(super) fn generation(&self) -> Option<u64> {
        self.policy
            .as_ref()
            .filter(|_| self.is_active())
            .map(|p| p.generation)
    }
    pub(super) fn worker_plan(self: &Arc<Self>) -> Result<super::origin_plan::WorkerPlan, String> {
        if !self.is_active() {
            return Err("native cache generation is retired".into());
        }
        let policy = self
            .policy
            .as_ref()
            .ok_or("native cache configuration is absent")?;
        let chunk = ramshared_block::chunk_bytes_from_env();
        let members = policy
            .members
            .iter()
            .map(|member| super::WorkerSpawnParams {
                target_bytes: member.target_bytes,
                chunk_bytes: chunk,
                reserve_floor: member.reserve_bytes,
                expected_adapter: Some(member.adapter.clone()),
                generation: Some(self.clone()),
                host_catalog: None,
            })
            .collect();
        Ok(super::origin_plan::WorkerPlan {
            chunk_bytes: chunk,
            members,
        })
    }
}
#[derive(Clone)]
pub(super) struct NativeCacheAdmission(pub(super) Arc<NativeCacheSession>);
impl ramshared_block::CacheAdmission for NativeCacheAdmission {
    fn admit_cache(&self) -> Result<(), String> {
        if self.0.is_active() && self.0.policy.is_some() {
            Ok(())
        } else {
            Err(self
                .0
                .unavailable_reason
                .clone()
                .unwrap_or_else(|| "native cache generation is unavailable or retired".into()))
        }
    }
}

fn load(origin: &NativeOriginManifest) -> Result<Option<CacheConfiguration>, String> {
    match std::fs::symlink_metadata(configuration_path()) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.to_string()),
        Ok(_) => {}
    }
    protected_path(&configuration_path(), 0)?;
    let bytes = super::manifest_file::read_bounded_manifest_file(&configuration_path(), true)?;
    let policy =
        CacheConfiguration::parse(std::str::from_utf8(&bytes).map_err(|e| e.to_string())?)?;
    verify(
        &policy,
        origin,
        &std::fs::read_link("/proc/self/exe").map_err(|e| e.to_string())?,
        0,
    )?;
    Ok(Some(policy))
}
fn verify(
    policy: &CacheConfiguration,
    origin: &NativeOriginManifest,
    image: &Path,
    owner: u32,
) -> Result<(), String> {
    policy.validate()?;
    if policy.origin != origin.target || image != policy.daemon_executable {
        return Err("native cache origin or installed daemon path differs".into());
    }
    protected_path(image, owner)?;
    verify_image(policy, image)
}
fn protected_path(path: &Path, owner: u32) -> Result<(), String> {
    for path in path.ancestors() {
        let metadata = std::fs::symlink_metadata(path).map_err(|e| e.to_string())?;
        if metadata.file_type().is_symlink()
            || ![0, owner].contains(&metadata.uid())
            || metadata.mode() & 0o022 != 0
        {
            return Err("native cache daemon hierarchy is not protected".into());
        }
    }
    Ok(())
}
fn verify_image(policy: &CacheConfiguration, image: &Path) -> Result<(), String> {
    let opened = rustix::fs::openat2(
        rustix::fs::CWD,
        image,
        rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
        rustix::fs::ResolveFlags::NO_SYMLINKS | rustix::fs::ResolveFlags::NO_MAGICLINKS,
    )
    .map_err(|e| e.to_string())?;
    let mut file = File::from(opened);
    let actual = File::open("/proc/self/exe")
        .map_err(|e| e.to_string())?
        .metadata()
        .map_err(|e| e.to_string())?;
    let before = file.metadata().map_err(|e| e.to_string())?;
    if !before.is_file() || before.dev() != actual.dev() || before.ino() != actual.ino() {
        return Err("native cache daemon is not the recorded installed image".into());
    }
    let mut bounded = (&mut file).take(
        before
            .len()
            .checked_add(1)
            .ok_or("native daemon image length overflows")?,
    );
    let mut digest = Sha256::new();
    let mut buffer = [0u8; 65536];
    loop {
        let len = bounded.read(&mut buffer).map_err(|e| e.to_string())?;
        if len == 0 {
            break;
        }
        digest.update(&buffer[..len]);
    }
    let hash: String = digest
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let after = file.metadata().map_err(|e| e.to_string())?;
    if hash != policy.daemon_sha256
        || before.len() != after.len()
        || before.mtime() != after.mtime()
        || before.mtime_nsec() != after.mtime_nsec()
    {
        return Err("native cache installed image changed or has a different hash".into());
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests;
