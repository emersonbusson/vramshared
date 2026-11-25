// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-winsvc/src/host_storage/tests.rs
use super::*;

fn test_storage_report() -> HostStorageReport {
    HostStorageReport {
        schema_version: 1,
        producer: crate::gpu_identity::ProducerProcess {
            pid: 1,
            creation_time_100ns: 1,
        },
        started_at_unix_ms: 1,
        elapsed_ms: 0,
        sample: HostVolumeSample {
            sampled_at_unix_ms: 1,
            volume_id: "volume".into(),
            file_identity: HostFileIdentity {
                volume_serial: 1,
                file_index: 1,
            },
            total_bytes: 100,
            free_bytes: 50,
            available_bytes: 40,
        },
        virtual_disk_id: Some([7; 16]),
    }
}

#[test]
fn disk_match_refuses_any_unreadable_candidate() {
    let report = test_storage_report();
    assert!(select_inspected_storage([7; 16], [Ok(report.clone())]).is_ok());
    assert!(
        select_inspected_storage([7; 16], [Ok(report.clone()), Err("unreadable".into())]).is_err()
    );
    assert!(
        select_inspected_storage([7; 16], [Err("unreadable".into()), Ok(report.clone())]).is_err()
    );
    let mut unknown = report.clone();
    unknown.virtual_disk_id = None;
    assert!(select_inspected_storage([7; 16], [Ok(report.clone()), Ok(unknown)]).is_err());
    assert!(select_inspected_storage([7; 16], [Ok(report.clone()), Ok(report)]).is_err());
}

#[test]
fn disk_inspection_refuses_registration_changes_and_failed_reobservation() {
    use std::{collections::VecDeque, path::PathBuf};

    let original = vec![(
        "sid-1".to_owned(),
        "guid".to_owned(),
        PathBuf::from("disk"),
        10,
    )];
    let moved = vec![(
        "sid-2".to_owned(),
        "guid".to_owned(),
        PathBuf::from("disk"),
        10,
    )];
    let mut stable = VecDeque::from([Ok(original.clone()), Ok(original.clone())]);
    assert!(
        inspect_stable_registration(
            [7; 16],
            || stable.pop_front().unwrap(),
            |_| Ok(test_storage_report())
        )
        .is_ok()
    );
    let mut changed = VecDeque::from([Ok(original.clone()), Ok(moved)]);
    assert!(
        inspect_stable_registration(
            [7; 16],
            || changed.pop_front().unwrap(),
            |_| Ok(test_storage_report())
        )
        .is_err()
    );
    let mut reverted = original.clone();
    reverted[0].3 += 1;
    let mut changed_timestamp = VecDeque::from([Ok(original.clone()), Ok(reverted)]);
    assert!(
        inspect_stable_registration(
            [7; 16],
            || changed_timestamp.pop_front().unwrap(),
            |_| Ok(test_storage_report())
        )
        .is_err()
    );
    let mut failed = VecDeque::from([Ok(original), Err("registry unavailable".into())]);
    assert!(
        inspect_stable_registration(
            [7; 16],
            || failed.pop_front().unwrap(),
            |_| Ok(test_storage_report())
        )
        .is_err()
    );
}

#[test]
fn volume_sample_rejects_inconsistent_or_unidentified_capacity() {
    let identity = || HostFileIdentity {
        volume_serial: 1,
        file_index: 1,
    };
    assert!(validate_sample(1, r"\\?\Volume{example}\", identity(), 100, 50, 40).is_some());
    assert!(validate_sample(0, r"\\?\Volume{example}\", identity(), 100, 50, 40).is_none());
    assert!(validate_sample(1, "", identity(), 100, 50, 40).is_none());
    assert!(validate_sample(1, r"\\?\Volume{example}\", identity(), 0, 0, 0).is_none());
    assert!(validate_sample(1, r"\\?\Volume{example}\", identity(), 100, 101, 40).is_none());
    assert!(validate_sample(1, r"\\?\Volume{example}\", identity(), 100, 50, 51).is_none());
}

#[cfg(windows)]
#[test]
fn native_file_sample_has_fixed_volume_identity_and_consistent_space() {
    let path =
        std::env::temp_dir().join(format!("ramshared-host-storage-{}.tmp", std::process::id()));
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .unwrap();
    let sample = sample_file(&path).expect("local temporary file volume is observable");
    drop(file);
    std::fs::remove_file(&path).unwrap();
    assert!(sample.volume_id.starts_with(r"\\?\Volume{"));
    assert!(sample.sampled_at_unix_ms > 0);
    assert!(sample.available_bytes <= sample.free_bytes);
    assert!(sample.free_bytes <= sample.total_bytes);
}

#[cfg(windows)]
#[test]
fn native_sample_keeps_open_file_identity_across_rename() {
    let dir = std::env::temp_dir();
    let path = dir.join(format!(
        "ramshared-storage-source-{}.tmp",
        std::process::id()
    ));
    let renamed = dir.join(format!(
        "ramshared-storage-renamed-{}.tmp",
        std::process::id()
    ));
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&path)
        .unwrap();
    let before = sample_open_file(&file).expect("sample original file");
    std::fs::rename(&path, &renamed).unwrap();
    let after = sample_open_file(&file).expect("sample same open file after rename");
    assert_eq!(before.file_identity, after.file_identity);
    assert_eq!(before.volume_id, after.volume_id);
    drop(file);
    std::fs::remove_file(&renamed).unwrap();
}

#[test]
fn handle_volume_root_requires_exact_guid_path() {
    let root = r"\\?\Volume{01234567-89ab-cdef-0123-456789abcdef}\";
    let valid: Vec<u16> = format!("{root}nested\\file.vhdx").encode_utf16().collect();
    assert_eq!(volume_guid_root(&valid), Some(root.to_string()));
    for bad in [
        r"C:\nested\file.vhdx",
        r"\\?\Volume{01234567-89ab-cdef-0123-456789abcdeg}\file",
        r"\\?\Volume{0123456789ab-cdef-0123-456789abcdef}\file",
        r"\\?\Volume{01234567-89ab-cdef-0123-456789abcdef}file",
    ] {
        assert!(volume_guid_root(&bad.encode_utf16().collect::<Vec<_>>()).is_none());
    }
}

#[test]
fn supervised_storage_report_rejects_invalid_provenance_and_timing() {
    let sample = validate_sample(
        101,
        r"\\?\Volume{01234567-89ab-cdef-0123-456789abcdef}\",
        HostFileIdentity {
            volume_serial: 1,
            file_index: 42,
        },
        100,
        50,
        40,
    )
    .unwrap();
    let report = HostStorageReport {
        schema_version: 1,
        producer: crate::gpu_identity::ProducerProcess {
            pid: 10,
            creation_time_100ns: 20,
        },
        started_at_unix_ms: 100,
        elapsed_ms: 2,
        sample,
        virtual_disk_id: Some([7; 16]),
    };
    let bytes = serde_json::to_vec(&report).unwrap();
    assert_eq!(decode_report(&bytes).unwrap(), report);
    let mut missing = report.clone();
    missing.virtual_disk_id = None;
    assert_eq!(
        decode_report(&serde_json::to_vec(&missing).unwrap()).unwrap(),
        missing
    );
    let mut invalid = report.clone();
    invalid.virtual_disk_id = Some([0; 16]);
    assert!(invalid.validate().is_err());
    invalid = report.clone();
    invalid.producer.pid = 0;
    assert!(invalid.validate().is_err());
    invalid = report.clone();
    invalid.sample.sampled_at_unix_ms = 103;
    assert!(invalid.validate().is_err());
    invalid = report.clone();
    invalid.sample.available_bytes = 51;
    assert!(invalid.validate().is_err());
    invalid = report.clone();
    invalid.sample.volume_id = "I:".into();
    assert!(invalid.validate().is_err());
    invalid = report.clone();
    invalid.started_at_unix_ms = u64::MAX;
    assert!(invalid.validate().is_err());
    assert!(decode_report(&vec![b' '; MAX_STORAGE_REPORT_BYTES + 1]).is_err());
}

#[test]
fn supervised_candidate_requires_requested_disk_and_original_freshness() {
    use ramshared_ipc::storage_proof::{MAX_STORAGE_AGE_MS, StorageSession};
    use ramshared_ipc::{HandshakeTranscript, HostChallenge};
    let transcript = HandshakeTranscript {
        min_version: 2,
        max_version: 5,
        boot_id: "boot".into(),
        distro_id: "guest".into(),
        guest_nonce: [1; 32],
    };
    let challenge = HostChallenge {
        accepted_version: 5,
        heartbeat_secs: 5,
        lease_timeout_secs: 20,
        host_nonce: [2; 32],
    };
    let mut guest = StorageSession::new(b"secret", &transcript, &challenge, 7).unwrap();
    let mut host = StorageSession::new(b"secret", &transcript, &challenge, 7).unwrap();
    let bytes = guest.request(b"secret", [7; 16]).unwrap();
    let request = host.verify_request(b"secret", &bytes).unwrap();
    let mut report = HostStorageReport {
        schema_version: 1,
        producer: crate::gpu_identity::ProducerProcess {
            pid: 10,
            creation_time_100ns: 20,
        },
        started_at_unix_ms: 100,
        elapsed_ms: 2,
        sample: validate_sample(
            101,
            r"\\?\Volume{01234567-89ab-cdef-0123-456789abcdef}\",
            HostFileIdentity {
                volume_serial: 1,
                file_index: 42,
            },
            100,
            50,
            40,
        )
        .unwrap(),
        virtual_disk_id: Some([7; 16]),
    };
    let selected = candidate::from_supervised_report(&request, &report, 125).unwrap();
    assert_eq!(
        (selected.disk_id, selected.available_bytes, selected.age_ms),
        ([7; 16], 40, 125)
    );
    assert!(candidate::from_supervised_report(&request, &report, MAX_STORAGE_AGE_MS).is_none());
    assert!(candidate::from_supervised_report(&request, &report, 1).is_none());
    report.virtual_disk_id = Some([8; 16]);
    assert!(candidate::from_supervised_report(&request, &report, 125).is_none());
    report.virtual_disk_id = Some([7; 16]);
    report.sample.available_bytes = 101;
    assert!(candidate::from_supervised_report(&request, &report, 125).is_none());
    report.sample.available_bytes = 40;
    let frame = host
        .seal_requested(b"secret", request, true, vec![selected])
        .unwrap();
    let now = std::time::Instant::now();
    let accepted = guest
        .open(b"secret", [7; 16], &frame, now, now, 1000)
        .unwrap();
    assert_eq!(
        accepted
            .candidate_for([7; 16], now, |lease| lease == 7)
            .unwrap()
            .available_bytes,
        40
    );
    assert!(accepted.candidate_for([7; 16], now, |_| false).is_none());
}

#[test]
fn shared_supervisor_checks_storage_report_process_and_window() {
    use crate::gpu_identity::supervisor::{
        ExitObservation, PipeRead, ProbeSupervisor, ProbeTick, Producer,
    };
    use std::time::{Duration, Instant};
    struct FinishedProducer {
        identity: crate::gpu_identity::ProducerProcess,
        output: Option<Vec<u8>>,
    }
    impl Producer for FinishedProducer {
        fn identity(&self) -> Result<crate::gpu_identity::ProducerProcess, String> {
            Ok(self.identity.clone())
        }
        fn observe_exit(&mut self) -> Result<ExitObservation, String> {
            Ok(ExitObservation {
                code: Some(0),
                job_empty: true,
            })
        }
        fn read_stdout(&mut self, buffer: &mut [u8]) -> Result<PipeRead, String> {
            match self.output.take() {
                Some(bytes) => {
                    buffer[..bytes.len()].copy_from_slice(&bytes);
                    Ok(PipeRead::Bytes(bytes.len()))
                }
                None => Ok(PipeRead::Closed),
            }
        }
        fn read_stderr(&mut self, _: &mut [u8]) -> Result<PipeRead, String> {
            Ok(PipeRead::Closed)
        }
        fn terminate_job(&mut self) -> Result<(), String> {
            panic!("finished producer must not be terminated")
        }
    }
    let producer = crate::gpu_identity::ProducerProcess {
        pid: 17,
        creation_time_100ns: 19,
    };
    let report = HostStorageReport {
        schema_version: 1,
        producer: producer.clone(),
        started_at_unix_ms: 101,
        elapsed_ms: 1,
        virtual_disk_id: Some([7; 16]),
        sample: validate_sample(
            102,
            r"\\?\Volume{01234567-89ab-cdef-0123-456789abcdef}\",
            HostFileIdentity {
                volume_serial: 1,
                file_index: 42,
            },
            100,
            50,
            40,
        )
        .unwrap(),
    };
    for matches in [true, false] {
        let now = Instant::now();
        let mut supervisor = ProbeSupervisor::<FinishedProducer, HostStorageReport>::default();
        let mut identity = producer.clone();
        if !matches {
            identity.pid += 1;
        }
        let output = serde_json::to_vec(&report).unwrap();
        supervisor
            .start(
                || {
                    Ok(FinishedProducer {
                        identity,
                        output: Some(output),
                    })
                },
                now,
                100,
                Duration::from_secs(1),
            )
            .unwrap();
        assert!(matches!(
            supervisor.tick(now + Duration::from_millis(2), 103),
            ProbeTick::Running
        ));
        match supervisor.tick(now + Duration::from_millis(3), 103) {
            ProbeTick::Completed(result) => assert_eq!(result.is_ok(), matches),
            other => panic!("unexpected supervisor result: {other:?}"),
        }
        assert_eq!(
            supervisor
                .snapshot(now + Duration::from_millis(4))
                .is_some(),
            matches
        );
    }
}

#[cfg(windows)]
#[test]
fn native_storage_producer_reports_its_process_and_observation_window() {
    let path = std::env::temp_dir().join(format!(
        "ramshared-storage-producer-{}.tmp",
        std::process::id()
    ));
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .unwrap();
    let report = inspect_file(&path).expect("storage producer report");
    drop(file);
    std::fs::remove_file(&path).unwrap();
    report.validate().unwrap();
    assert_eq!(report.producer.pid, std::process::id());
    assert_ne!(report.producer.creation_time_100ns, 0);
    assert!(report.sample.sampled_at_unix_ms >= report.started_at_unix_ms);
}

#[cfg(windows)]
#[test]
fn pinned_observer_file_cannot_be_renamed_until_custody_ends() {
    let dir = std::env::temp_dir();
    let path = dir.join(format!("ramshared-pin-{}.tmp", std::process::id()));
    let renamed = dir.join(format!("ramshared-pin-renamed-{}.tmp", std::process::id()));
    let creator = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .unwrap();
    drop(creator);
    let pinned = open_pinned(&path).expect("pinned file");
    assert!(std::fs::rename(&path, &renamed).is_err());
    drop(pinned);
    std::fs::rename(&path, &renamed).unwrap();
    std::fs::remove_file(&renamed).unwrap();
}

#[test]
fn select_inspected_storage_refuses_zero_and_missing_disk_ids() {
    let err = select_inspected_storage([0; 16], std::iter::empty()).unwrap_err();
    assert!(err.contains("zero disk identity"));
    let err = select_inspected_storage([7; 16], std::iter::empty()).unwrap_err();
    assert!(err.contains("no registered storage match"));
}

#[cfg(not(windows))]
#[test]
fn non_windows_storage_commands_fail_closed() {
    assert_eq!(inspect_command(std::path::Path::new("/nonexistent")), 2);
    assert_eq!(inspect_disk_command([7; 16]), 1);
}
