// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/main_tests/native_data.rs
#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::*;
use crate::native_fixture::Fixture;
use ramshared_block::native_origin::{NativeOriginVolume, OriginMount};

#[test]
fn native_origin_args_refuse_mixed_and_uncertain_authority() {
    let path = ramshared_config::native_origin::default_native_origin_path();
    let path = path.to_str().unwrap();
    let args = AppArgs::parse_from(&daemon_argv(&[
        "ramsharedd",
        "--native-origin-manifest",
        path,
    ]))
    .unwrap();
    assert_eq!(args.native_origin.as_ref().unwrap().expected_bytes, None);
    assert!(select_daemon_action_for(args, Some(false)).is_ok());
    for measured in [None, Some(true)] {
        let args = AppArgs::parse_from(&daemon_argv(&[
            "ramsharedd",
            "--native-origin-manifest",
            path,
            "--accept-wsl2-nbd",
        ]))
        .unwrap();
        assert!(select_daemon_action_for(args, measured).is_err());
    }
    for extra in [
        vec!["--origin-manifest", ORIGIN_MANIFEST_PATH],
        vec!["--native-origin-manifest", path],
        vec!["--transport", "ublk"],
        vec!["--zram-pump-tier", "/dev/zram0"],
        vec!["--size", "0"],
        vec!["--force"],
    ] {
        let mut values = daemon_argv(&["ramsharedd", "--native-origin-manifest", path]);
        values.extend(extra.into_iter().map(str::to_owned));
        assert!(AppArgs::parse_from(&values).is_err());
    }
    assert!(
        AppArgs::parse_from(&daemon_argv(&[
            "ramsharedd",
            "--native-origin-manifest",
            "/tmp/caller.toml"
        ]))
        .is_err()
    );
    let args = AppArgs::parse_from(&daemon_argv(&[
        "ramsharedd",
        "--native-origin-manifest",
        path,
        "--size",
        "4",
    ]))
    .unwrap();
    assert_eq!(
        args.native_origin.as_ref().unwrap().expected_bytes,
        Some(4 * 1024 * 1024)
    );
}

struct NativeStarter {
    client: Option<std::thread::JoinHandle<()>>,
    statuses: Vec<(bool, &'static str, &'static str)>,
}
impl NbdRuntimeStarter for NativeStarter {
    fn lock_memory(&mut self, force: bool, future: bool) -> Result<(), Box<dyn std::error::Error>> {
        assert!(!force && !future);
        Ok(())
    }
    fn start_acceptor(
        &mut self,
        _listener: UnixListener,
        exports: Arc<Vec<ramshared_block::handshake::Export>>,
        _flags: u16,
        jobs: std::sync::mpsc::SyncSender<WMsg>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        assert_eq!(exports[0].size, 8192);
        self.client = Some(std::thread::spawn(move || {
            // Every reply is checked before issuing the dependent request.
            let result = std::panic::catch_unwind(|| {
                jobs.send(WMsg::Opened).unwrap();
                for (command, handle, length, payload, flags) in [
                    (Command::Write, 31, BLOCK_SIZE, vec![47; 4096], 1),
                    (Command::Read, 32, BLOCK_SIZE, vec![], 0),
                    (Command::Flush, 33, 0, vec![], 0),
                ] {
                    let (tx, rx) = std::sync::mpsc::channel();
                    jobs.send(WMsg::Job(ramshared_wsl2d::conn::Job {
                        export: 0,
                        req: ramshared_block::Request {
                            flags,
                            cmd: command,
                            handle,
                            offset: 0,
                            len: length,
                        },
                        payload,
                        credit: fixture_credit(),
                        reply: tx,
                    }))
                    .unwrap();
                    let reply = rx.recv_timeout(Duration::from_secs(5)).unwrap();
                    assert_eq!(
                        u32::from_be_bytes(reply.reply[4..8].try_into().unwrap()),
                        ramshared_block::protocol::NBD_OK
                    );
                    assert_eq!(
                        u64::from_be_bytes(reply.reply[8..16].try_into().unwrap()),
                        handle
                    );
                    if command == Command::Read {
                        assert_eq!(reply.data, [47; 4096]);
                    }
                }
            });
            let _ = jobs.send(WMsg::Shutdown);
            if let Err(panic) = result {
                std::panic::resume_unwind(panic);
            }
        }));
        Ok(())
    }
    fn nbd_used_kb(&mut self, _: &str) -> u64 {
        0
    }
    fn nbd_swap_is_explicitly_absent(&mut self, _: &str) -> bool {
        true
    }
    fn try_claim_shutdown(&mut self, _: &str) -> Option<ShutdownPermit> {
        Some(ShutdownPermit::injected(()))
    }
    fn publish_demote(&mut self, _: &DemoteStatusFile) {}
    fn publish_origin_cache(&mut self, status: &OriginCacheStatus) {
        self.statuses
            .push((status.ok, status.origin_state, status.cache_state));
    }
    fn elapsed_us(&mut self, _: Instant) -> u64 {
        0
    }
    fn spawn_swapoff(&mut self, _: &str) -> std::sync::mpsc::Receiver<SwapoffOutcome> {
        panic!("inactive native fixture")
    }
    fn spawn_recovery_activation(
        &mut self,
        _: &str,
        _: i16,
    ) -> Result<std::sync::mpsc::Receiver<bool>, Box<dyn std::error::Error>> {
        panic!("no activation authority")
    }
    fn startup_budget(
        &mut self,
        _: bool,
    ) -> Result<Option<Box<dyn NbdBudgetProvider>>, Box<dyn std::error::Error>> {
        panic!("no DXG on native origin")
    }
    fn global_free_bytes(&mut self, _: Duration) -> Option<u64> {
        panic!("native origin serving must not query a GPU provider")
    }
    fn origin_cache_target_bytes(&mut self, _: u64) -> Result<u64, Box<dyn std::error::Error>> {
        panic!("no native cache cap applied")
    }
    fn spawn_isolated_gpu_worker(
        &mut self,
        params: &WorkerSpawnParams,
    ) -> worker_startup::InitialStartResult {
        let (_, _, _) = (
            params.target_bytes,
            params.chunk_bytes,
            params.reserve_floor,
        );
        panic!("no native GPU admission")
    }
}

#[test]
fn native_origin_nbd_runtime_serves_retained_file_without_gpu_or_swap_activation() {
    let fixture = Fixture::new();
    let mount = &fixture.devices[0].mounts[0];
    let volume = NativeOriginVolume::open(
        &OriginMount {
            mountpoint: mount.mountpoint.clone(),
            mount_id: mount.mount_id,
            major_minor: mount.major_minor.clone(),
        },
        fixture.uid,
    )
    .unwrap();
    let file = volume.verify_for_io(&fixture.manifest.target).unwrap();
    let mut starter = NativeStarter {
        client: None,
        statuses: vec![],
    };
    let path = fixture.root.join("native.sock");
    run_origin_nbd_with_startup(
        UnavailableVramProvider,
        Some(OpenedOrigin::Native(crate::native_origin::AdmittedOrigin {
            file: FileOrigin::from_file(file),
            logical_bytes: 8192,
            manifest: fixture.manifest.clone(),
            cache_session: None,
        })),
        8192,
        path.to_str().unwrap().into(),
        false,
        "/dev/ramshared-native-fixture".into(),
        2,
        &mut starter,
    )
    .unwrap();
    starter.client.take().unwrap().join().unwrap();
    assert_eq!(
        &std::fs::read(fixture.origin_path()).unwrap()[..4096],
        &[47; 4096]
    );
    assert_eq!(
        &std::fs::read(fixture.origin_path()).unwrap()[4096..],
        &[17; 4096]
    );
    assert!(starter.statuses.contains(&(true, "READY", "UNAVAILABLE")));
    assert!(!path.exists());
    assert!(
        volume.verify_for_io(&fixture.manifest.target).is_ok(),
        "shutdown released the origin's own I/O lease"
    );
}
