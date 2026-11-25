// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/host_storage_state.rs
//! Shared authenticated Windows host storage state for cache status and
//! WSL2 emergency swap headroom bounding (IPC v5).

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use ramshared_block::vhd_identity::{RootVhdObservation, observe_directory_vhd};
use ramshared_ipc::storage_proof::{
    AuthenticatedStorage, HostStorageTelemetry, StorageCandidate, bound_wsl_storage_free_bytes,
    format_disk_id_hex,
};

use crate::host_gate::SessionAuthority;
use crate::host_storage_match::match_directory_observation;

/// Verified root-VHD identity paired with its authenticated Windows host sample.
pub struct AuthenticatedHostStorage {
    pub directory: RootVhdObservation,
    pub storage: AuthenticatedStorage,
}

struct StoredHostStorage {
    authority: Option<SessionAuthority>,
    observation: AuthenticatedHostStorage,
}

impl StoredHostStorage {
    fn session_matches(&self, candidate: Option<&SessionAuthority>) -> bool {
        match (&self.authority, candidate) {
            (Some(current), Some(caller)) => !current.is_current() || current.same_session(caller),
            (None, None) | (None, Some(_)) => true,
            (Some(current), None) => !current.is_current(),
        }
    }

    fn verified_candidate(
        &self,
        directory: &RootVhdObservation,
        lease_lost: bool,
        now: Instant,
    ) -> Option<&StorageCandidate> {
        if lease_lost {
            return None;
        }
        let authority = self.authority.as_ref()?;
        if !authority.is_current() {
            return None;
        }
        match_directory_observation(
            directory,
            &self.observation.storage,
            true,
            now,
            |lease_id| authority.lease_is_current(lease_id),
        )
    }
}

/// Parent-owned live Windows storage proof shared between `VsockControlPlane`,
/// `/run/ramshared/cache-status.json` publication, and `tier_release`.
#[derive(Clone, Default)]
pub struct SharedHostStorage {
    inner: Arc<Mutex<Option<StoredHostStorage>>>,
}

impl SharedHostStorage {
    pub fn new() -> Self {
        Self::default()
    }

    /// Store a newly authenticated host storage sample if this session is current.
    pub fn publish(
        &self,
        authority: Option<SessionAuthority>,
        observation: AuthenticatedHostStorage,
    ) {
        self.publish_after(authority, observation, || {});
    }

    fn publish_after(
        &self,
        authority: Option<SessionAuthority>,
        observation: AuthenticatedHostStorage,
        before_lock: impl FnOnce(),
    ) {
        before_lock();
        if let Ok(mut guard) = self.inner.lock() {
            if !authority.as_ref().is_some_and(SessionAuthority::is_current) {
                return;
            }
            *guard = Some(StoredHostStorage {
                authority,
                observation,
            });
        }
    }

    /// Revoke stored host storage unless a newer current session replaced it.
    pub fn revoke_for(&self, authority: Option<&SessionAuthority>) {
        if let Ok(mut guard) = self.inner.lock()
            && guard
                .as_ref()
                .is_some_and(|stored| stored.session_matches(authority))
        {
            *guard = None;
        }
    }

    /// Snapshot current host storage telemetry if valid, unexpired, and leased.
    pub fn telemetry(&self, lease_lost: bool) -> Option<HostStorageTelemetry> {
        self.telemetry_at(lease_lost, Instant::now())
    }

    fn telemetry_at(&self, lease_lost: bool, now: Instant) -> Option<HostStorageTelemetry> {
        let guard = self.inner.try_lock().ok()?;
        let stored = guard.as_ref()?;
        let candidate =
            stored.verified_candidate(&stored.observation.directory, lease_lost, now)?;
        let authority = stored.authority.as_ref()?;
        let remaining = stored
            .observation
            .storage
            .remaining_lifetime_ms(now, |lease_id| authority.lease_is_current(lease_id))?;
        let deadline = stored
            .observation
            .storage
            .current_boot_deadline(now, |lease_id| authority.lease_is_current(lease_id))?
            .clone();
        let age = ramshared_ipc::storage_proof::MAX_STORAGE_AGE_MS.checked_sub(remaining)?;
        Some(HostStorageTelemetry {
            disk_id: format_disk_id_hex(&candidate.disk_id),
            volume_id: candidate.volume_id.clone(),
            total_bytes: candidate.total_bytes,
            available_bytes: candidate.available_bytes,
            age_ms: age.max(candidate.age_ms),
            deadline,
        })
    }

    /// Compute WSL2-bounded free space for `root` given `guest_free` bytes.
    pub fn bounded_free_bytes_with(
        &self,
        root: &Path,
        guest_free: u64,
        lease_lost: bool,
        observe_dir: impl FnOnce(&Path) -> Option<RootVhdObservation>,
    ) -> Option<u64> {
        let dir_obs = observe_dir(root)?;
        let guard = self.inner.try_lock().ok()?;
        let stored = guard.as_ref()?;
        let candidate = stored.verified_candidate(&dir_obs, lease_lost, Instant::now())?;
        Some(bound_wsl_storage_free_bytes(
            guest_free,
            candidate.available_bytes,
        ))
    }

    /// Production helper binding `root` via kernel VPD Page 83 VHD inspection.
    pub fn bounded_free_bytes(
        &self,
        root: &Path,
        guest_free: u64,
        lease_lost: bool,
    ) -> Option<u64> {
        self.bounded_free_bytes_with(root, guest_free, lease_lost, observe_directory_vhd)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;
    use crate::host_gate::{ControlPlaneAuthority, LeaseToken, SealedOrigin, SharedAuthority};
    use ramshared_ipc::storage_proof::storage_boot_clock;
    use ramshared_ipc::storage_proof::{StorageCandidate, StorageSession};
    use ramshared_ipc::storage_transport::{receive_response, respond_to_request, send_request};
    use ramshared_ipc::{HandshakeTranscript, HostChallenge};
    use std::io::Cursor;
    use std::time::Duration;

    fn sealed_origin() -> SealedOrigin {
        SealedOrigin {
            logical_capacity_mib: 4096,
            partuuid: "00000000-0000-0000-0000-000000000001".into(),
            origin_vhdx: "/origin.vhdx".into(),
        }
    }

    struct MemStream {
        rx: Cursor<Vec<u8>>,
        tx: Vec<u8>,
    }
    impl std::io::Read for MemStream {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.rx.read(buf)
        }
    }
    impl std::io::Write for MemStream {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.tx.write(buf)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn authenticated_pair(
        disk_id: [u8; 16],
        available_bytes: u64,
    ) -> (StorageSession, AuthenticatedStorage) {
        let transcript = HandshakeTranscript {
            min_version: 2,
            max_version: 5,
            boot_id: "boot".into(),
            distro_id: "ubuntu".into(),
            guest_nonce: [1; 32],
        };
        let challenge = HostChallenge {
            accepted_version: 5,
            heartbeat_secs: 1,
            lease_timeout_secs: 15,
            host_nonce: [2; 32],
        };
        let mut guest = StorageSession::new(b"secret", &transcript, &challenge, 7).unwrap();
        let mut host = StorageSession::new(b"secret", &transcript, &challenge, 7).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut req = Vec::new();
        let pending = send_request(&mut req, &mut guest, b"secret", disk_id, deadline).unwrap();
        let mut stream = MemStream {
            rx: Cursor::new(req),
            tx: Vec::new(),
        };
        respond_to_request(&mut stream, &mut host, b"secret", deadline, |id| {
            Ok((
                true,
                vec![StorageCandidate {
                    disk_id: id.disk_id(),
                    volume_id: r"\\?\Volume{01234567-89ab-cdef-0123-456789abcdef}\".into(),
                    file_serial: 1,
                    file_index: 42,
                    total_bytes: 100 << 30,
                    available_bytes,
                    age_ms: 10,
                }],
            ))
        })
        .unwrap();
        let auth =
            receive_response(&mut Cursor::new(stream.tx), &mut guest, b"secret", pending).unwrap();
        (guest, auth)
    }

    #[test]
    fn queued_old_publication_cannot_replace_new_session() {
        let authorities = SharedAuthority::new();
        let old = authorities.begin_session();
        let disk_id = [0x77; 16];
        let (_old_session, old_storage) = authenticated_pair(disk_id, 2 << 30);
        let (_new_session, new_storage) = authenticated_pair(disk_id, 9 << 30);
        let directory = RootVhdObservation {
            major: 8,
            minor: 64,
            identifier: disk_id,
            observed_at_unix_ms: 1_000,
            observed_at: Instant::now(),
        };
        let store = SharedHostStorage::new();
        let mut guard = store.inner.lock().unwrap();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let worker_store = store.clone();
        let old_directory = directory.clone();
        let worker = std::thread::spawn(move || {
            worker_store.publish_after(
                Some(old),
                AuthenticatedHostStorage {
                    directory: old_directory,
                    storage: old_storage,
                },
                || started_tx.send(()).unwrap(),
            );
        });
        started_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        let new = authorities.begin_session();
        let mut authority = ControlPlaneAuthority::disconnected();
        authority
            .accept_lease(
                LeaseToken {
                    lease_id: 7,
                    deadline_ms: u64::MAX / 2,
                },
                sealed_origin(),
                100,
            )
            .unwrap();
        assert!(new.publish(&authority));
        *guard = Some(StoredHostStorage {
            authority: Some(new),
            observation: AuthenticatedHostStorage {
                directory,
                storage: new_storage,
            },
        });
        drop(guard);
        worker.join().unwrap();
        assert_eq!(store.telemetry(false).unwrap().available_bytes, 9 << 30);
    }

    #[test]
    fn shared_host_storage_bounds_wsl_free_space_and_fences_stale_sessions() {
        let shared_auth = SharedAuthority::new();
        let session = shared_auth.begin_session();
        let mut auth = ControlPlaneAuthority::disconnected();
        auth.accept_lease(
            LeaseToken {
                lease_id: 7,
                deadline_ms: u64::MAX / 2,
            },
            sealed_origin(),
            100,
        )
        .unwrap();
        assert!(session.publish(&auth));

        let disk_id = [0x99u8; 16];
        let (_keep_alive, auth_storage) = authenticated_pair(disk_id, 5 << 30);
        let dir = RootVhdObservation {
            major: 8,
            minor: 64,
            identifier: disk_id,
            observed_at_unix_ms: 1000,
            observed_at: Instant::now(),
        };

        let store = SharedHostStorage::new();
        assert!(
            store
                .bounded_free_bytes(Path::new("/nonexistent-ramshared-test"), 1 << 30, false)
                .is_none()
        );
        store.publish(
            Some(session.clone()),
            AuthenticatedHostStorage {
                directory: dir.clone(),
                storage: auth_storage,
            },
        );
        let telemetry = store.telemetry(false).expect("live telemetry");
        assert_eq!(telemetry.available_bytes, 5 << 30);
        assert_eq!(telemetry.disk_id, format_disk_id_hex(&disk_id));
        let (boot, boot_ns) = storage_boot_clock().unwrap();
        assert!(telemetry.deadline.is_current(&boot, boot_ns));
        assert!(
            !telemetry
                .deadline
                .is_current(&boot, boot_ns + 5_000_000_000)
        );
        let observed = Instant::now();
        let early = store.telemetry_at(false, observed).expect("initial proof");
        let late = store
            .telemetry_at(false, observed + Duration::from_secs(2))
            .expect("proof still leased");
        assert!(late.age_ms >= early.age_ms + 2_000);

        assert_eq!(
            store.bounded_free_bytes_with(Path::new("/"), 794 << 30, false, |_| Some(dir.clone())),
            Some(4 << 30)
        );
        assert_eq!(
            store.bounded_free_bytes_with(Path::new("/"), 2 << 30, false, |_| Some(dir.clone())),
            Some(2 << 30)
        );
        let wrong = RootVhdObservation {
            major: 8,
            minor: 64,
            identifier: [0x11u8; 16],
            observed_at_unix_ms: 1000,
            observed_at: Instant::now(),
        };
        assert_eq!(
            store
                .bounded_free_bytes_with(Path::new("/mnt/other"), 50 << 30, false, |_| Some(wrong)),
            None
        );
        assert!(store.telemetry(true).is_none());
        assert!(
            store
                .bounded_free_bytes_with(Path::new("/"), 794 << 30, true, |_| Some(dir.clone()))
                .is_none()
        );

        let newer = shared_auth.begin_session();
        let mut auth2 = ControlPlaneAuthority::disconnected();
        auth2
            .accept_lease(
                LeaseToken {
                    lease_id: 7,
                    deadline_ms: u64::MAX / 2,
                },
                sealed_origin(),
                101,
            )
            .unwrap();
        assert!(newer.publish(&auth2));
        let (_keep2, auth_storage2) = authenticated_pair(disk_id, 9 << 30);
        // Superseded session publish is ignored and never overwrites active slot.
        let (_keep_stale, auth_stale) = authenticated_pair(disk_id, 1 << 30);
        store.publish(
            Some(session.clone()),
            AuthenticatedHostStorage {
                directory: dir.clone(),
                storage: auth_stale,
            },
        );
        assert!(store.telemetry(false).is_none());

        store.publish(
            Some(newer.clone()),
            AuthenticatedHostStorage {
                directory: dir.clone(),
                storage: auth_storage2,
            },
        );
        let (_keep_late, auth_late) = authenticated_pair(disk_id, 2 << 30);
        store.publish(
            Some(session.clone()),
            AuthenticatedHostStorage {
                directory: dir.clone(),
                storage: auth_late,
            },
        );
        assert_eq!(store.telemetry(false).unwrap().available_bytes, 9 << 30);
        let (_keep_unbound, auth_unbound) = authenticated_pair(disk_id, 3 << 30);
        store.publish(
            None,
            AuthenticatedHostStorage {
                directory: dir.clone(),
                storage: auth_unbound,
            },
        );
        assert_eq!(store.telemetry(false).unwrap().available_bytes, 9 << 30);
        store.revoke_for(None);
        store.revoke_for(Some(&session));
        assert_eq!(store.telemetry(false).unwrap().available_bytes, 9 << 30);
        store.revoke_for(Some(&newer));
        assert!(store.telemetry(false).is_none());
    }
}
