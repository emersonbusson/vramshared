// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/native_cache/tests.rs
//! Controlled configuration authority; no installed/native hardware qualification.
#![allow(clippy::unwrap_used)]
use super::*;
use crate::native_fixture::Fixture;
use ramshared_block::CacheAdmission;
use ramshared_config::cache_configuration::CacheMemberConfiguration;
use ramshared_vram::{GpuAdapterIdentity, RUNTIME_FREE_BUFFER_BYTES, ReserveFloorPolicy};
use std::os::unix::fs::PermissionsExt;
use std::sync::atomic::AtomicUsize;

pub(crate) fn policy(origin: &NativeOriginManifest) -> CacheConfiguration {
    CacheConfiguration {
        schema_version: 1,
        generation: 7,
        plan_fingerprint: "a".repeat(64),
        approval_id: "native-test".into(),
        approval_issued_unix: 1000,
        applied_unix: 1001,
        profile_fingerprint: "b".repeat(64),
        release_version: "synthetic-release".into(),
        daemon_executable: std::env::current_exe().unwrap(),
        daemon_sha256: "c".repeat(64),
        origin: origin.target.clone(),
        lower_sink_identity: "synthetic-sink".into(),
        required_lower_sink_bytes: 512 << 20,
        members: vec![1, 2]
            .into_iter()
            .map(|key| CacheMemberConfiguration {
                adapter: GpuAdapterIdentity {
                    backend: "cuda".into(),
                    key: format!("{key:032x}"),
                    luid: None,
                    node_mask: None,
                },
                capacity_bytes: 6 << 30,
                target_bytes: 4096,
                reserve_bytes: ReserveFloorPolicy::sealed().configured_reserve_bytes(6 << 30),
                runtime_buffer_bytes: RUNTIME_FREE_BUFFER_BYTES,
            })
            .collect(),
    }
}
pub(crate) fn synthetic_session(policy: CacheConfiguration) -> Arc<NativeCacheSession> {
    Arc::new(NativeCacheSession {
        lease: Some(File::open("/dev/null").unwrap()),
        policy: Some(policy),
        unavailable_reason: None,
        active: AtomicBool::new(true),
    })
}
pub(crate) struct LeaseFixture(pub(crate) std::path::PathBuf);
impl LeaseFixture {
    pub(crate) fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(2)
            .unwrap()
            .join("local/tests/cache_generation")
            .join(format!(
                "{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::SeqCst)
            ));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        Self(root)
    }
    pub(crate) fn path(&self) -> std::path::PathBuf {
        self.0.join("configuration.lock")
    }
    pub(crate) fn uid(&self) -> u32 {
        rustix::process::geteuid().as_raw()
    }
}
impl Drop for LeaseFixture {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).unwrap();
    }
}

#[test]
fn native_cache_matches_origin_image_and_held_generation() {
    let origin = Fixture::new();
    let fixture = LeaseFixture::new();
    let mut config = policy(&origin.manifest);
    let mut actual = File::open("/proc/self/exe").unwrap();
    let mut digest = Sha256::new();
    let mut bytes = [0; 65536];
    loop {
        let n = actual.read(&mut bytes).unwrap();
        if n == 0 {
            break;
        }
        digest.update(&bytes[..n]);
    }
    config.daemon_sha256 = digest
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    verify_image(&config, &config.daemon_executable).unwrap();
    let session =
        NativeCacheSession::open_with(&fixture.path(), fixture.uid(), || Ok(Some(config.clone())))
            .unwrap();
    assert!(
        NativeCacheSession::open_with(&fixture.path(), fixture.uid(), || panic!(
            "held generation must refuse before metadata read"
        ))
        .is_err()
    );
    let admission = NativeCacheAdmission(session.clone());
    assert!(admission.admit_cache().is_ok());
    assert_eq!(session.generation(), Some(7));
    let plan = session.worker_plan().unwrap();
    assert_eq!(plan.members.len(), 2);
    assert!(
        plan.members
            .iter()
            .all(|member| Arc::ptr_eq(member.generation.as_ref().unwrap(), &session))
    );
    assert!(
        plan.prepare(8192, ReserveFloorPolicy::sealed().min_floor_bytes)
            .is_ok()
    );
    session.revoke();
    assert!(admission.admit_cache().is_err());
    drop(admission);
    drop(session);
    assert!(NativeCacheSession::open_with(&fixture.path(), fixture.uid(), || Ok(None)).is_ok());
    config.daemon_sha256 = "d".repeat(64);
    assert!(verify_image(&config, &config.daemon_executable).is_err());
    assert!(verify_image(&config, Path::new("/dev/null")).is_err());
}
#[test]
fn native_cache_refuses_foreign_record_without_revoking_origin() {
    let origin = Fixture::new();
    let fixture = LeaseFixture::new();
    let config = policy(&origin.manifest);
    for result in [Ok(None), Err("foreign configuration".into())] {
        let session =
            NativeCacheSession::open_with(&fixture.path(), fixture.uid(), || result).unwrap();
        assert!(NativeCacheAdmission(session.clone()).admit_cache().is_err());
        assert!(session.unavailable_reason.is_some());
        assert!(session.worker_plan().is_err());
    }
    assert!(
        verify(
            &config,
            &origin.manifest,
            Path::new("/not-the-image"),
            fixture.uid()
        )
        .is_err()
    );
    let mut foreign = origin.manifest.clone();
    foreign.target = config.origin.clone();
    if let ramshared_config::resource_profile::ResourceTarget::LinuxFileOrigin { inode, .. } =
        &mut foreign.target
    {
        *inode += 1;
    }
    assert!(verify(&config, &foreign, &config.daemon_executable, fixture.uid()).is_err());
    assert!(
        verify(
            &config,
            &origin.manifest,
            &config.daemon_executable,
            u32::MAX
        )
        .is_err()
    );
    let original = std::fs::read(origin.origin_path()).unwrap();
    assert_eq!(original, [vec![17u8; 4096], vec![17u8; 4096]].concat());
}
