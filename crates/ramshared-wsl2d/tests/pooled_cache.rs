// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/tests/pooled_cache.rs
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use ramshared_block::isolated_origin::{
    POOLED_PAGE_BYTES, PooledCacheClient, PooledLookupReply, PooledReadTelemetry,
};
use ramshared_ipc::cache_wire::{
    CacheFrameBuffer, CacheFrameHeader, CacheGuestRole, CacheOpcode, CacheScope,
    DEFAULT_POOLED_CACHE_PORT, crc32c, read_into, validate_cache_data_port, write_frame_with,
};
use ramshared_wsl2d::pooled_cache::profile::{
    CACHE_PROFILE_SCHEMA_VERSION, CacheProfile, CacheProfileSelection, ProfileError,
    StagedCacheProfile, load_boot_profile, parse_staged_profile, stage_next_boot_profile,
};
use ramshared_wsl2d::pooled_cache::{
    EffectiveCacheMode, GuestBootError, GuestCacheSession, HostBootstrapProof, StreamPooledClient,
    VMADDR_CID_HOST, verify_exclusive_origin,
};

fn unique_temp_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "ramshared-wsl2d-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn sample_doc(profile: CacheProfile) -> StagedCacheProfile {
    StagedCacheProfile {
        schema_version: CACHE_PROFILE_SCHEMA_VERSION,
        profile,
        role: "wsl2".into(),
        endpoint_version: 1,
        data_port: DEFAULT_POOLED_CACHE_PORT,
        key_ref: "/etc/ramshared/pooled.key".into(),
        release_binding: "v1.0.4".into(),
        origin_binding: "5039dca1-61a0-41ff-aa08-221f49326a1b".into(),
        generation: 1,
    }
}

#[test]
fn cache_profile_stage_is_idempotent_and_refuses_active_switch() {
    let dir = unique_temp_dir("profile-stage");
    let path = dir.join("cache-mode.json");

    // Absent file defaults cleanly to Independent.
    assert_eq!(
        load_boot_profile(&path).unwrap(),
        CacheProfileSelection::DefaultIndependent
    );
    assert_eq!(
        CacheProfileSelection::DefaultIndependent.profile(),
        CacheProfile::Independent
    );
    assert_eq!(
        CacheProfile::parse("independent").unwrap(),
        CacheProfile::Independent
    );
    assert_eq!(CacheProfile::parse("pooled").unwrap(), CacheProfile::Pooled);
    assert_eq!(
        CacheProfile::parse("hybrid"),
        Err(ProfileError::InvalidMode)
    );
    assert_eq!(CacheProfile::Independent.as_str(), "independent");
    assert_eq!(CacheProfile::Pooled.as_str(), "pooled");

    // Refuses active in-place switch while an origin is already serving (#13).
    assert_eq!(
        stage_next_boot_profile(&path, sample_doc(CacheProfile::Pooled), true),
        Err(ProfileError::ActiveSwitchRefused)
    );
    assert!(!path.exists(), "refused active switch must not write state");

    // First next-boot stage commits generation 1; duplicate staging is idempotent (2x = 1x, #17).
    let mut initial = sample_doc(CacheProfile::Pooled);
    initial.data_port = 0; // defaults to DEFAULT_POOLED_CACHE_PORT
    let first = stage_next_boot_profile(&path, initial, false).unwrap();
    assert_eq!(first.generation, 1);
    assert_eq!(first.data_port, DEFAULT_POOLED_CACHE_PORT);

    let second = stage_next_boot_profile(&path, sample_doc(CacheProfile::Pooled), false).unwrap();
    assert_eq!(
        second, first,
        "duplicate staging must keep exact generation"
    );

    // Switching back to Independent increments generation monotonically.
    let reverted =
        stage_next_boot_profile(&path, sample_doc(CacheProfile::Independent), false).unwrap();
    assert_eq!(reverted.generation, 2);
    assert_eq!(reverted.profile, CacheProfile::Independent);
    let loaded = load_boot_profile(&path).unwrap();
    assert_eq!(loaded.profile(), CacheProfile::Independent);

    // Malformed / oversized / unknown-field / bad-version / colliding-port documents are refused.
    assert!(matches!(
        parse_staged_profile(&"x".repeat(9000)),
        Err(ProfileError::MalformedDocument(_))
    ));
    assert!(matches!(
        parse_staged_profile(r#"{"unknown":true}"#),
        Err(ProfileError::MalformedDocument(_))
    ));
    let mut bad_ver = sample_doc(CacheProfile::Pooled);
    bad_ver.schema_version = 99;
    assert_eq!(
        bad_ver.validate(),
        Err(ProfileError::UnsupportedVersion(99))
    );
    let mut bad_ep = sample_doc(CacheProfile::Pooled);
    bad_ep.endpoint_version = 2;
    assert!(bad_ep.validate().is_err());
    let mut bad_role = sample_doc(CacheProfile::Pooled);
    bad_role.role = "unknown".into();
    assert!(bad_role.validate().is_err());
    let mut bad_port = sample_doc(CacheProfile::Pooled);
    bad_port.data_port = 2761;
    assert!(bad_port.validate().is_err());
    let mut bad_meta = sample_doc(CacheProfile::Pooled);
    bad_meta.key_ref = "   ".into();
    assert!(bad_meta.validate().is_err());

    let mut sec = sample_doc(CacheProfile::Pooled);
    sec.role = "secondary_guest".into();
    assert_eq!(sec.validate().unwrap(), CacheGuestRole::SecondaryGuest);

    // Path/IO refusal coverage: reading a directory, missing parent, missing filename, missing dir.
    assert!(matches!(load_boot_profile(&dir), Err(ProfileError::Io(_))));
    assert!(matches!(
        stage_next_boot_profile(
            std::path::Path::new("/"),
            sample_doc(CacheProfile::Pooled),
            false
        ),
        Err(ProfileError::Io(_))
    ));
    assert!(matches!(
        stage_next_boot_profile(&dir.join(".."), sample_doc(CacheProfile::Pooled), false),
        Err(ProfileError::Io(_))
    ));
    assert!(matches!(
        stage_next_boot_profile(
            &dir.join("nonexistent-subdir/cache-mode.json"),
            sample_doc(CacheProfile::Pooled),
            false
        ),
        Err(ProfileError::Io(_))
    ));

    let _ = format!("{}", ProfileError::StaleBinding);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn pooled_boot_retains_origin_when_host_is_absent_and_refuses_aliased_swap() {
    // 1. Aliased swap/origin backing identity is refused fail-closed (DT-2 / #13).
    assert_eq!(
        verify_exclusive_origin("", &["/dev/sdc"]),
        Err(GuestBootError::AliasedSwapBacking("empty".into()))
    );
    let err = GuestCacheSession::from_boot_selection(
        Ok(CacheProfileSelection::Staged(sample_doc(
            CacheProfile::Pooled,
        ))),
        "6b1632b8-4b03-462a-90f2-f5de50b4ff5d",
        &["/dev/sdc", "6B1632B8-4B03-462A-90F2-F5DE50B4FF5D"],
        None,
    )
    .err()
    .unwrap();
    assert!(err.to_string().contains("aliased"));
    assert_eq!(err, err.clone());
    assert!(!format!("{err:?}").is_empty());

    // 2. Pooled mode with absent host endpoint retains local origin, NEVER spawns independent GPU worker.
    let absent_host = GuestCacheSession::from_boot_selection(
        Ok(CacheProfileSelection::Staged(sample_doc(
            CacheProfile::Pooled,
        ))),
        "5039dca1-61a0-41ff-aa08-221f49326a1b",
        &["/dev/sdc", "ci-swap-10g.vhdx"],
        None,
    )
    .unwrap();
    assert!(!absent_host.spawns_independent_worker());
    assert_eq!(absent_host.active_epoch(), 0);
    let st = absent_host.status(None);
    assert_eq!(st.configured_profile, CacheProfile::Pooled);
    assert_eq!(st.effective_mode, EffectiveCacheMode::PooledOriginOnly);
    assert_eq!(st.effective_mode.as_str(), "pooled_origin_only");
    assert_eq!(st.reason.as_deref(), Some("host_cache_endpoint_absent"));

    // 3. Malformed explicit config also falls back to origin-only without spawning independent worker.
    let malformed = GuestCacheSession::from_boot_selection(
        Err(ProfileError::MalformedDocument("corrupt".into())),
        "5039dca1-61a0-41ff-aa08-221f49326a1b",
        &["/dev/sdc"],
        None,
    )
    .unwrap();
    assert!(!malformed.spawns_independent_worker());
    assert_eq!(
        malformed.status(None).effective_mode,
        EffectiveCacheMode::PooledOriginOnly
    );
    assert_eq!(malformed.status(None).peer_refusals, 1);

    // 4. Independent default and explicit Independent keep independent worker policy.
    for sel in [
        CacheProfileSelection::DefaultIndependent,
        CacheProfileSelection::Staged(sample_doc(CacheProfile::Independent)),
    ] {
        let indep = GuestCacheSession::from_boot_selection(
            Ok(sel),
            "5039dca1-61a0-41ff-aa08-221f49326a1b",
            &["/dev/sdc"],
            None,
        )
        .unwrap();
        assert!(indep.spawns_independent_worker());
        assert_eq!(indep.status(None).effective_mode.as_str(), "independent");
    }

    // 5. Wrong host CID or invalid host HMAC proof refuses pooled activation cleanly.
    let key = [0x66u8; 32];
    let scope = CacheScope {
        role: CacheGuestRole::Wsl2,
        runtime_id: [0x11; 16],
        service_guid: validate_cache_data_port(DEFAULT_POOLED_CACHE_PORT, 2761).unwrap(),
        origin_fingerprint: [0x22; 32],
        epoch: 4,
        extent_bytes: 16 * POOLED_PAGE_BYTES as u64,
        guest_nonce: [0x33; 32],
        host_nonce: [0x44; 32],
    };
    let good_proof = scope.host_proof(&key).unwrap();
    let bad_cid = GuestCacheSession::from_boot_selection(
        Ok(CacheProfileSelection::Staged(sample_doc(
            CacheProfile::Pooled,
        ))),
        "5039dca1-61a0-41ff-aa08-221f49326a1b",
        &["/dev/sdc"],
        Some(HostBootstrapProof {
            host_cid: 99,
            scope: scope.clone(),
            host_proof: good_proof,
            key,
        }),
    )
    .unwrap();
    assert_eq!(
        bad_cid.status(None).effective_mode,
        EffectiveCacheMode::PooledOriginOnly
    );

    // 6. Authenticated PooledActive session + real UnixStream RSCP v1 roundtrip + retirement.
    let proof = HostBootstrapProof {
        host_cid: VMADDR_CID_HOST,
        scope: scope.clone(),
        host_proof: good_proof,
        key,
    };
    assert_eq!(proof, proof.clone());
    assert!(!format!("{proof:?}").is_empty());
    let active = GuestCacheSession::from_boot_selection(
        Ok(CacheProfileSelection::Staged(sample_doc(
            CacheProfile::Pooled,
        ))),
        "5039dca1-61a0-41ff-aa08-221f49326a1b",
        &["/dev/sdc"],
        Some(proof),
    )
    .unwrap();
    assert!(!active.spawns_independent_worker());
    assert_eq!(active.active_epoch(), 4);
    assert_eq!(active.scope(), Some(&scope));
    assert_eq!(active.status(None).effective_mode.as_str(), "pooled_active");

    let (client_sock, mut host_sock) = UnixStream::pair().unwrap();
    let page = [0x4Du8; POOLED_PAGE_BYTES];
    let server = std::thread::spawn(move || {
        let mut rbuf = CacheFrameBuffer::new();
        let mut wbuf = CacheFrameBuffer::new();
        // Serve 1 Offer -> Accepted, 1 Lookup -> Hit, 1 Lookup -> Miss, 1 Lookup -> Busy.
        let offer = read_into(&mut host_sock, &mut rbuf).unwrap();
        assert_eq!(offer.header.opcode, CacheOpcode::Offer);
        let stored = *offer.page.unwrap();
        let acc = CacheFrameHeader::new(
            CacheOpcode::Accepted,
            offer.header.correlation,
            offer.header.epoch,
            offer.header.offset_bytes,
            offer.header.content_gen,
        )
        .unwrap();
        write_frame_with(&mut host_sock, &mut wbuf, acc, None).unwrap();

        for (op, with_page) in [
            (CacheOpcode::Hit, true),
            (CacheOpcode::Miss, false),
            (CacheOpcode::Busy, false),
        ] {
            let req = read_into(&mut host_sock, &mut rbuf).unwrap();
            let rep = CacheFrameHeader::new(
                op,
                req.header.correlation,
                req.header.epoch,
                req.header.offset_bytes,
                req.header.content_gen,
            )
            .unwrap();
            write_frame_with(&mut host_sock, &mut wbuf, rep, with_page.then_some(&stored)).unwrap();
        }
    });

    let wire_client = StreamPooledClient::new(scope, client_sock);
    assert!(wire_client.offer_copy(4, 0, 1, &page, crc32c(&page)));
    let mut dst = [0u8; POOLED_PAGE_BYTES];
    let deadline = Instant::now() + Duration::from_secs(2);
    assert_eq!(
        wire_client.lookup_into(4, 0, 1, 7, deadline, &mut dst),
        PooledLookupReply::Hit {
            epoch: 4,
            slot_gen: 7,
            content_gen: 1,
            crc: crc32c(&page),
        }
    );
    assert_eq!(dst, page);
    assert_eq!(
        wire_client.lookup_into(4, 4096, 1, 8, deadline, &mut dst),
        PooledLookupReply::Miss
    );
    assert_eq!(
        wire_client.lookup_into(4, 8192, 1, 9, deadline, &mut dst),
        PooledLookupReply::Busy
    );
    server.join().unwrap();

    wire_client.close_poisoned(4);
    assert!(wire_client.is_closed());
    assert!(!wire_client.offer_copy(4, 0, 1, &page, crc32c(&page)));
    assert_eq!(
        wire_client.lookup_into(4, 0, 1, 10, deadline, &mut dst),
        PooledLookupReply::TimedOut
    );

    active.retire_to_origin_only("peer_closed");
    // Second retire is idempotent (covers prev == 0 and non-active branches).
    active.retire_to_origin_only("peer_closed");
    let final_st = active.status(Some(PooledReadTelemetry {
        hits: 1,
        misses: 1,
        origin_fallbacks: 2,
        ..PooledReadTelemetry::default()
    }));
    assert_eq!(
        final_st.effective_mode,
        EffectiveCacheMode::PooledOriginOnly
    );
    assert_eq!(final_st.generation_changes, 2);
    assert_eq!(final_st.origin_fallback_bytes, 2 * POOLED_PAGE_BYTES as u64);
    let encoded_st = serde_json::to_string(&final_st).unwrap();
    let decoded_st: ramshared_wsl2d::pooled_cache::PooledGuestStatus =
        serde_json::from_str(&encoded_st).unwrap();
    assert_eq!(decoded_st, final_st.clone());
    assert!(!format!("{decoded_st:?}").is_empty());

    // 7. StreamPooledClient transport & protocol refusal coverage (write err, EOF, mismatch, Offer->Busy).
    struct BrokenStream {
        fail_write: bool,
        reply: Vec<u8>,
        pos: usize,
    }
    impl std::io::Read for BrokenStream {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.pos >= self.reply.len() {
                return Ok(0);
            }
            let n = buf.len().min(self.reply.len() - self.pos);
            buf[..n].copy_from_slice(&self.reply[self.pos..self.pos + n]);
            self.pos += n;
            Ok(n)
        }
    }
    impl std::io::Write for BrokenStream {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            if self.fail_write {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "write fail",
                ));
            }
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    let c_write = StreamPooledClient::new(
        active.scope().unwrap().clone(),
        BrokenStream {
            fail_write: true,
            reply: Vec::new(),
            pos: 0,
        },
    );
    // Invalid epoch=0 header fails before sending.
    assert_eq!(
        c_write.lookup_into(0, 0, 1, 1, deadline, &mut dst),
        PooledLookupReply::TransportError
    );
    assert!(!c_write.offer_copy(0, 0, 1, &page, crc32c(&page)));
    // Write failure poisons lookup.
    assert_eq!(
        c_write.lookup_into(4, 0, 1, 1, deadline, &mut dst),
        PooledLookupReply::TransportError
    );
    assert!(c_write.is_closed());

    let c_offer_write = StreamPooledClient::new(
        active.scope().unwrap().clone(),
        BrokenStream {
            fail_write: true,
            reply: Vec::new(),
            pos: 0,
        },
    );
    assert!(!c_offer_write.offer_copy(4, 0, 1, &page, crc32c(&page)));
    assert!(c_offer_write.is_closed());

    // EOF on lookup read poisons client.
    let c_eof = StreamPooledClient::new(
        active.scope().unwrap().clone(),
        BrokenStream {
            fail_write: false,
            reply: Vec::new(),
            pos: 0,
        },
    );
    assert_eq!(
        c_eof.lookup_into(4, 0, 1, 1, deadline, &mut dst),
        PooledLookupReply::TransportError
    );
    assert!(c_eof.is_closed());

    // Mismatched reply header on lookup poisons client.
    let mut bad_reply = Vec::new();
    let mut wb = CacheFrameBuffer::new();
    let bad_hdr = CacheFrameHeader::new(CacheOpcode::Miss, 99, 4, 0, 1).unwrap();
    write_frame_with(&mut bad_reply, &mut wb, bad_hdr, None).unwrap();
    let c_mismatch = StreamPooledClient::new(
        active.scope().unwrap().clone(),
        BrokenStream {
            fail_write: false,
            reply: bad_reply,
            pos: 0,
        },
    );
    assert_eq!(
        c_mismatch.lookup_into(4, 0, 1, 1, deadline, &mut dst),
        PooledLookupReply::TransportError
    );
    assert!(c_mismatch.is_closed());

    // Valid Busy reply on offer returns false without closing; subsequent EOF closes.
    let mut busy_reply = Vec::new();
    let busy_hdr = CacheFrameHeader::new(CacheOpcode::Busy, 1, 4, 0, 1).unwrap();
    write_frame_with(&mut busy_reply, &mut wb, busy_hdr, None).unwrap();
    let c_offer_busy = StreamPooledClient::new(
        active.scope().unwrap().clone(),
        BrokenStream {
            fail_write: false,
            reply: busy_reply,
            pos: 0,
        },
    );
    assert!(!c_offer_busy.offer_copy(4, 0, 1, &page, crc32c(&page)));
    assert!(!c_offer_busy.is_closed());
    assert!(!c_offer_busy.offer_copy(4, 0, 1, &page, crc32c(&page)));
    assert!(c_offer_busy.is_closed());

    // Contended StreamPooledClient try_lock returns Busy on lookup and false on offer without blocking.
    let gate = std::sync::Arc::new((
        std::sync::Mutex::new((false, false)),
        std::sync::Condvar::new(),
    ));
    struct BlockingReadStream {
        gate: std::sync::Arc<(std::sync::Mutex<(bool, bool)>, std::sync::Condvar)>,
    }
    impl std::io::Read for BlockingReadStream {
        fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
            let (lock, cvar) = &*self.gate;
            let mut st = lock.lock().unwrap();
            st.0 = true;
            cvar.notify_all();
            while !st.1 {
                st = cvar.wait(st).unwrap();
            }
            Ok(0)
        }
    }
    impl std::io::Write for BlockingReadStream {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let contended = std::sync::Arc::new(StreamPooledClient::new(
        active.scope().unwrap().clone(),
        BlockingReadStream {
            gate: std::sync::Arc::clone(&gate),
        },
    ));
    let holder = {
        let c = std::sync::Arc::clone(&contended);
        std::thread::spawn(move || {
            let mut tmp = [0u8; POOLED_PAGE_BYTES];
            let _ = c.lookup_into(
                4,
                0,
                1,
                1,
                Instant::now() + Duration::from_secs(2),
                &mut tmp,
            );
        })
    };
    {
        let (lock, cvar) = &*gate;
        let mut st = lock.lock().unwrap();
        while !st.0 {
            st = cvar.wait(st).unwrap();
        }
    }
    assert_eq!(
        contended.lookup_into(
            4,
            0,
            1,
            1,
            Instant::now() + Duration::from_secs(2),
            &mut dst
        ),
        PooledLookupReply::Busy
    );
    assert!(!contended.offer_copy(4, 0, 1, &page, crc32c(&page)));
    {
        let (lock, cvar) = &*gate;
        let mut st = lock.lock().unwrap();
        st.1 = true;
        cvar.notify_all();
    }
    holder.join().unwrap();
}

#[test]
fn guest_connector_bootstraps_live_rscp_session_and_falls_back_on_refusals() {
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use ramshared_block::isolated_origin::AuthoritativeOriginBackend;
    use ramshared_block::{DisabledCache, FileOrigin};
    use ramshared_ipc::vsock::{MockVsockStream, VsockPeerIdentity, linux_service_guid};
    use ramshared_vram::ReserveFloorPolicy;
    use ramshared_winsvc::pooled_cache::allocation::{
        AdapterObservation, GuestCustodyState, GuestPlacementIntent, PoolAdmission, RolePlacement,
    };
    use ramshared_winsvc::pooled_cache::server::{
        SchedulerLoopConfig, create_scheduler_channel, run_scheduler_loop, serve_session_stream,
    };
    use ramshared_winsvc::pooled_cache::worker::{GpuWorkerBackend, OwnedGpuWorker, WorkerFault};
    use ramshared_winsvc::pooled_cache::{EnrolledRole, PoolSupervisor};
    use ramshared_wsl2d::pooled_cache::connector::{
        connect_boot_pooled_with_dialer, parse_key_bytes, read_pooled_key,
    };

    struct MemWorker(Vec<[u8; POOLED_PAGE_BYTES]>);
    impl GpuWorkerBackend for MemWorker {
        fn store_slot(
            &mut self,
            slot: usize,
            page: &[u8; POOLED_PAGE_BYTES],
        ) -> Result<(), WorkerFault> {
            self.0[slot] = *page;
            Ok(())
        }
        fn load_slot(
            &mut self,
            slot: usize,
            dst: &mut [u8; POOLED_PAGE_BYTES],
        ) -> Result<(), WorkerFault> {
            *dst = self.0[slot];
            Ok(())
        }
        fn scrub_slot(&mut self, slot: usize) -> Result<(), WorkerFault> {
            self.0[slot].fill(0);
            Ok(())
        }
        fn terminate_process(&mut self) {}
        fn context_destroyed(&self) -> bool {
            true
        }
    }

    let dir = unique_temp_dir("connector-e2e");
    let key = [0x7Du8; 32];
    let key_path = dir.join("pooled.key");
    std::fs::write(&key_path, key).unwrap();
    std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(read_pooled_key(&key_path).unwrap(), key);

    // Hex key + refusal cases (mode 0644, symlink, all-zero, bad length).
    let hex_path = dir.join("hex.key");
    let hex_str: String = key.iter().map(|b| format!("{b:02x}")).collect();
    std::fs::write(&hex_path, format!("{hex_str}\n")).unwrap();
    std::fs::set_permissions(&hex_path, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(read_pooled_key(&hex_path).unwrap(), key);
    std::fs::set_permissions(&hex_path, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(read_pooled_key(&hex_path).is_err());
    let link_path = dir.join("link.key");
    std::os::unix::fs::symlink(&key_path, &link_path).unwrap();
    assert!(read_pooled_key(&link_path).is_err());
    assert!(read_pooled_key(&dir).is_err());
    let empty_key = dir.join("empty.key");
    std::fs::write(&empty_key, b"").unwrap();
    std::fs::set_permissions(&empty_key, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert!(read_pooled_key(&empty_key).is_err());
    assert!(parse_key_bytes(&[0u8; 32]).is_err());
    assert!(parse_key_bytes(b"short").is_err());
    assert!(parse_key_bytes(&[0xFF; 17]).is_err());
    assert!(parse_key_bytes(format!("{}zz", "aa".repeat(31)).as_bytes()).is_err());

    // Direct handshake refusals: rejected host challenge and tampered host HMAC proof.
    {
        use ramshared_ipc::cache_wire::{
            CacheHostChallenge, read_client_hello, write_host_challenge,
        };
        use ramshared_wsl2d::pooled_cache::connector::handshake_and_wrap;

        let (c_rej, mut s_rej) = UnixStream::pair().unwrap();
        let t1 = std::thread::spawn(move || {
            let _ = read_client_hello(&mut s_rej).unwrap();
            write_host_challenge(&mut s_rej, &CacheHostChallenge::rejected()).unwrap();
        });
        assert!(
            handshake_and_wrap(
                MockVsockStream(c_rej),
                CacheGuestRole::SecondaryGuest,
                DEFAULT_POOLED_CACHE_PORT,
                key,
                [0x22; 32],
                4096,
                [0x11; 32],
            )
            .is_err()
        );
        t1.join().unwrap();

        let (c_bad, mut s_bad) = UnixStream::pair().unwrap();
        let t2 = std::thread::spawn(move || {
            let _ = read_client_hello(&mut s_bad).unwrap();
            write_host_challenge(
                &mut s_bad,
                &CacheHostChallenge {
                    accepted: true,
                    epoch: 1,
                    runtime_id: [0x11; 16],
                    host_nonce: [0x22; 32],
                    host_proof: [0xEE; 32],
                },
            )
            .unwrap();
        });
        assert!(
            handshake_and_wrap(
                MockVsockStream(c_bad),
                CacheGuestRole::SecondaryGuest,
                DEFAULT_POOLED_CACHE_PORT,
                key,
                [0x22; 32],
                4096,
                [0x11; 32],
            )
            .is_err()
        );
        t2.join().unwrap();
    }

    // Absent boot profile path via connect_boot_pooled_session defaults to Independent.
    let (default_sess, default_state) =
        ramshared_wsl2d::pooled_cache::connector::connect_boot_pooled_session(
            &dir.join("absent-cache-mode.json"),
            "5039dca1-61a0-41ff-aa08-221f49326a1b",
            &[],
            4096,
        )
        .unwrap();
    assert!(default_state.is_none());
    assert!(default_sess.spawns_independent_worker());

    let origin_binding = "5039dca1-61a0-41ff-aa08-221f49326a1b";
    let fp = ramshared_ipc::cache_wire::origin_fingerprint_from_binding(origin_binding).unwrap();
    let vm_id = [0x99u8; 16];
    let extent = 16 * POOLED_PAGE_BYTES as u64;

    // Spin up real host PoolSupervisor scheduler thread.
    let mut sup = PoolSupervisor::new(
        PoolAdmission::new(ReserveFloorPolicy::sealed(), "cuda-0-test"),
        OwnedGpuWorker::new(MemWorker(vec![[0u8; POOLED_PAGE_BYTES]; 16]), 16),
    );
    sup.enroll(EnrolledRole {
        role: CacheGuestRole::SecondaryGuest,
        key,
        expected_runtime_id: vm_id,
        expected_origin_fingerprint: fp,
        extent_bytes: extent,
        data_port: DEFAULT_POOLED_CACHE_PORT,
        control_port: 2761,
    })
    .unwrap();
    let consented = RolePlacement {
        intent: GuestPlacementIntent::PooledSealed,
        custody: GuestCustodyState::RunningConsented { boot_gen: 1 },
    };
    sup.update_placement(consented, consented, false, Instant::now())
        .unwrap();

    let (handle, rx) = create_scheduler_channel();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_sched = Arc::clone(&stop);
    let sched = std::thread::spawn(move || {
        run_scheduler_loop(
            sup,
            SchedulerLoopConfig {
                adapter_id: "cuda-0-test",
                slot_capacity: 16,
                wsl_auto_vm: false,
                status_path: None,
            },
            rx,
            |_, now| {
                Some(AdapterObservation {
                    adapter_id: "cuda-0-test".into(),
                    adapter_local_bytes: 6 * 1024 * 1024 * 1024,
                    process_budget_bytes: 6 * 1024 * 1024 * 1024,
                    wddm_current_usage_bytes: 512 * 1024 * 1024,
                    cuda_free_bytes: 4 * 1024 * 1024 * 1024,
                    host_physical_ram_bytes: 32 * 1024 * 1024 * 1024,
                    host_available_ram_bytes: 12 * 1024 * 1024 * 1024,
                    staging_allowance_bytes: 16 * 1024 * 1024,
                    sampled_at: now,
                    signed_match: true,
                })
            },
            &stop_sched,
        );
    });

    let mut staged = sample_doc(CacheProfile::Pooled);
    staged.role = "secondary_guest".into();
    staged.key_ref = key_path.to_string_lossy().into_owned();
    staged.origin_binding = origin_binding.into();

    // 1. Live end-to-end handshake + AuthoritativeOriginBackend read/offer/hit over UnixStream.
    let (cli_sock, srv_sock) = UnixStream::pair().unwrap();
    let h_srv = handle.clone();
    let s_srv = Arc::clone(&stop);
    let peer_thread = std::thread::spawn(move || {
        let mut s = MockVsockStream(srv_sock);
        serve_session_stream(
            &mut s,
            &h_srv,
            DEFAULT_POOLED_CACHE_PORT,
            &s_srv,
            Some(VsockPeerIdentity {
                vm_id,
                service_id: linux_service_guid(DEFAULT_POOLED_CACHE_PORT),
            }),
        )
    });

    let (session, pooled_state) = connect_boot_pooled_with_dialer(
        Ok(CacheProfileSelection::Staged(staged.clone())),
        origin_binding,
        &["/dev/sdb"],
        extent,
        |port| {
            assert_eq!(port, DEFAULT_POOLED_CACHE_PORT);
            Ok(MockVsockStream(cli_sock))
        },
    )
    .unwrap();
    assert_eq!(
        session.status(None).effective_mode,
        EffectiveCacheMode::PooledActive
    );
    let pooled_state = pooled_state.expect("live pooled state");

    let origin_file_path = dir.join("origin.img");
    let origin_file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&origin_file_path)
        .unwrap();
    origin_file.set_len(extent).unwrap();
    let backend = AuthoritativeOriginBackend::new(
        FileOrigin::from_file(origin_file),
        DisabledCache,
        extent,
        POOLED_PAGE_BYTES as u32,
    )
    .unwrap()
    .with_pooled_read(pooled_state);

    let pattern = [0xC3u8; POOLED_PAGE_BYTES];
    backend.write_at(4096, &pattern).unwrap();
    let mut read1 = [0u8; POOLED_PAGE_BYTES];
    // First read misses pooled cache, reads SSD origin, and offers page into host VRAM pool.
    backend.read_at(4096, &mut read1).unwrap();
    assert_eq!(read1, pattern);
    // Second read hits host VRAM pool directly!
    let mut read2 = [0u8; POOLED_PAGE_BYTES];
    backend.read_at(4096, &mut read2).unwrap();
    assert_eq!(read2, pattern);

    let tel = backend.pooled_read().unwrap().telemetry();
    assert_eq!(tel.misses, 1);
    assert_eq!(tel.offers_submitted, 1);
    assert_eq!(tel.hits, 1);
    let host_snap = handle.snapshot().unwrap();
    assert_eq!(host_snap.secondary.accepted_offers, 1);
    assert_eq!(host_snap.secondary.hits, 1);

    drop(backend);
    peer_thread.join().unwrap().unwrap();

    // 2. Origin binding mismatch and dial failure both fall back cleanly to PooledOriginOnly.
    let (mismatch_sess, mismatch_state) = connect_boot_pooled_with_dialer(
        Ok(CacheProfileSelection::Staged(staged.clone())),
        "different-origin-uuid",
        &[],
        extent,
        |_| Ok(MockVsockStream(UnixStream::pair().unwrap().0)),
    )
    .unwrap();
    assert!(mismatch_state.is_none());
    assert_eq!(
        mismatch_sess.status(None).effective_mode,
        EffectiveCacheMode::PooledOriginOnly
    );
    assert_eq!(
        mismatch_sess.status(None).reason.as_deref(),
        Some("origin_binding_mismatch")
    );

    let (dial_err_sess, dial_err_state) = connect_boot_pooled_with_dialer(
        Ok(CacheProfileSelection::Staged(staged)),
        origin_binding,
        &[],
        extent,
        |_| Err::<MockVsockStream, String>("host offline".into()),
    )
    .unwrap();
    assert!(dial_err_state.is_none());
    assert_eq!(
        dial_err_sess.status(None).effective_mode,
        EffectiveCacheMode::PooledOriginOnly
    );

    // 3. DefaultIndependent skips dialer cleanly.
    let (indep_sess, indep_state) = connect_boot_pooled_with_dialer(
        Ok(CacheProfileSelection::DefaultIndependent),
        origin_binding,
        &[],
        extent,
        |_| Err::<MockVsockStream, String>("must not dial".into()),
    )
    .unwrap();
    assert!(indep_state.is_none());
    assert!(indep_sess.spawns_independent_worker());

    // 4. Live `connect_boot_pooled_session` entrypoints (absent profile -> independent,
    //    staged secondary_guest on unused vsock port -> clean PooledOriginOnly fallback).
    let absent_path = dir.join("missing-profile.toml");
    let (absent_sess, absent_state) =
        ramshared_wsl2d::pooled_cache::connector::connect_boot_pooled_session(
            &absent_path,
            origin_binding,
            &[],
            extent,
        )
        .unwrap();
    assert!(absent_state.is_none());
    assert!(absent_sess.spawns_independent_worker());

    let unused_port_path = dir.join("unused-port-profile.toml");
    let unused_doc = StagedCacheProfile {
        schema_version: CACHE_PROFILE_SCHEMA_VERSION,
        profile: CacheProfile::Pooled,
        role: "secondary_guest".into(),
        endpoint_version: 1,
        data_port: 27699,
        key_ref: key_path.to_string_lossy().into_owned(),
        release_binding: "v1.0.4".into(),
        origin_binding: origin_binding.into(),
        generation: 1,
    };
    stage_next_boot_profile(&unused_port_path, unused_doc, false).unwrap();
    let (unreach_sess, unreach_state) =
        ramshared_wsl2d::pooled_cache::connector::connect_boot_pooled_session(
            &unused_port_path,
            origin_binding,
            &[],
            extent,
        )
        .unwrap();
    assert!(unreach_state.is_none());
    assert_eq!(
        unreach_sess.status(None).effective_mode,
        EffectiveCacheMode::PooledOriginOnly
    );

    stop.store(true, Ordering::Release);
    handle.shutdown();
    sched.join().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}
