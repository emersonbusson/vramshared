// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-ipc/src/bin/vsock-guest-lab.rs
//! Lab-only guest-side handshake/lease client over a host-initiated AF_VSOCK
//! connection. This proves the inverted native transport and the DT-3 lease
//! without starting the cache worker.

use std::io::{Read, Write};

use ramshared_ipc::{
    Handshake, HandshakeAck, HandshakeFinish, HandshakeTranscript, Heartbeat, HostChallenge,
    IPC_MIN_VERSION, IPC_VERSION_3, LeaseGranted, LeaseRequest, MAX_CONTROL_PAYLOAD, MSG_HANDSHAKE,
    MSG_HANDSHAKE_ACK, MSG_HANDSHAKE_FINISH, MSG_HEARTBEAT, MSG_HEARTBEAT_ACK, MSG_LEASE_GRANTED,
    MSG_LEASE_RELEASE, MSG_LEASE_REQUEST, decode_control, encode_control, guest_finish_proof,
    guest_proof, random_nonce, read_frame, verify_host_proof, write_frame,
};

fn parse_args(args: &[String]) -> Result<(u32, String), String> {
    let mut port = 2761u32;
    let mut key_path = "/etc/ramshared/vsock.key".to_string();
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--port" => {
                index += 1;
                port = args
                    .get(index)
                    .and_then(|value| value.parse().ok())
                    .ok_or_else(|| "invalid --port".to_string())?;
            }
            "--key" => {
                index += 1;
                key_path = args
                    .get(index)
                    .cloned()
                    .ok_or_else(|| "missing --key".to_string())?;
            }
            other => return Err(format!("unknown option: {other}")),
        }
        index += 1;
    }
    Ok((port, key_path))
}

fn parse_distro_id(os_release: &str) -> String {
    os_release
        .lines()
        .find_map(|line| line.strip_prefix("ID=").map(str::to_string))
        .unwrap_or_else(|| "unknown".to_string())
}

fn run_session<S: Read + Write>(
    stream: &mut S,
    secret: &[u8],
    boot_id: String,
    distro_id: String,
) -> Result<(u32, u64, u64), Box<dyn std::error::Error>> {
    let nonce = random_nonce()?;
    let transcript = HandshakeTranscript {
        min_version: IPC_MIN_VERSION,
        max_version: IPC_VERSION_3,
        boot_id: boot_id.clone(),
        distro_id: distro_id.clone(),
        guest_nonce: nonce,
    };
    let opening = Handshake {
        min_version: transcript.min_version,
        max_version: transcript.max_version,
        boot_id,
        distro_id,
        guest_nonce: nonce,
        guest_proof: guest_proof(secret, &transcript)?,
    };
    write_frame(stream, MSG_HANDSHAKE, &encode_control(&opening)?, 1)?;
    let (message, payload, _) = read_frame(stream, MAX_CONTROL_PAYLOAD)?;
    if message != MSG_HANDSHAKE_ACK {
        return Err(format!("expected handshake ack, got {message}").into());
    }
    let ack: HandshakeAck = decode_control(&payload)?;
    let challenge = HostChallenge {
        accepted_version: ack.accepted_version,
        heartbeat_secs: ack.heartbeat_secs,
        lease_timeout_secs: ack.lease_timeout_secs,
        host_nonce: ack.host_nonce,
    };
    if !verify_host_proof(secret, &transcript, &challenge, &ack.host_proof) {
        return Err("host proof mismatch".into());
    }
    let finish = HandshakeFinish {
        guest_finish_proof: guest_finish_proof(secret, &transcript, &challenge)?,
    };
    write_frame(stream, MSG_HANDSHAKE_FINISH, &encode_control(&finish)?, 2)?;
    let request = LeaseRequest {
        nonce: nonce.to_vec(),
    };
    write_frame(stream, MSG_LEASE_REQUEST, &encode_control(&request)?, 3)?;
    let (message, payload, _) = read_frame(stream, MAX_CONTROL_PAYLOAD)?;
    if message != MSG_LEASE_GRANTED {
        return Err(format!("expected lease grant, got {message}").into());
    }
    let granted: LeaseGranted = decode_control(&payload)?;
    let beat = Heartbeat {
        timestamp_ms: granted.deadline_ms.saturating_sub(10_000),
    };
    write_frame(stream, MSG_HEARTBEAT, &encode_control(&beat)?, 4)?;
    let (message, payload, _) = read_frame(stream, MAX_CONTROL_PAYLOAD)?;
    if message != MSG_HEARTBEAT_ACK {
        return Err(format!("expected heartbeat ack, got {message}").into());
    }
    let ack: ramshared_ipc::HeartbeatAck = decode_control(&payload)?;
    let _ = write_frame(stream, MSG_LEASE_RELEASE, b"", 4);
    Ok((
        granted.lease_id,
        granted.deadline_ms,
        ack.lease_remaining_ms,
    ))
}

#[cfg(not(test))]
fn run() -> Result<(), Box<dyn std::error::Error>> {
    use ramshared_ipc::vsock::listen_vsock;
    use std::time::Duration;
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (port, key_path) = parse_args(&args)?;
    let secret = std::fs::read(key_path)?;
    let boot_id = std::fs::read_to_string("/proc/sys/kernel/random/boot_id")?
        .trim()
        .to_string();
    let distro_id = parse_distro_id(&std::fs::read_to_string("/etc/os-release")?);
    println!("GUEST_LAB_LISTEN port={port}");
    let mut stream = listen_vsock(port, Duration::from_secs(30))?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    println!("GUEST_LAB_ACCEPT");
    let (lease_id, deadline_ms, remaining_ms) =
        run_session(&mut stream, &secret, boot_id, distro_id)?;
    println!("GUEST_LAB_LEASE lease_id={lease_id} deadline_ms={deadline_ms}");
    println!("GUEST_LAB_HEARTBEAT remaining_ms={remaining_ms}");
    println!("GUEST_LAB_PASS");
    Ok(())
}

#[cfg(not(test))]
fn main() {
    if let Err(error) = run() {
        eprintln!("vsock-guest-lab: {error}");
        std::process::exit(1);
    }
}

#[cfg(test)]
fn main() {}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use ramshared_ipc::{HeartbeatAck, host_proof, verify_guest_finish_proof, verify_guest_proof};

    #[test]
    fn parses_args_and_distro_id_or_refuses_invalid_options() {
        let (port, key) = parse_args(&[]).unwrap();
        assert_eq!(port, 2761);
        assert_eq!(key, "/etc/ramshared/vsock.key");
        let (port, key) = parse_args(&[
            "--port".into(),
            "2900".into(),
            "--key".into(),
            "/tmp/k".into(),
        ])
        .unwrap();
        assert_eq!(port, 2900);
        assert_eq!(key, "/tmp/k");
        assert!(parse_args(&["--port".into(), "bad".into()]).is_err());
        assert!(parse_args(&["--key".into()]).is_err());
        assert!(parse_args(&["--unknown".into()]).is_err());
        assert_eq!(parse_distro_id("NAME=Ubuntu\nID=ubuntu\n"), "ubuntu");
        assert_eq!(parse_distro_id("NAME=Custom\n"), "unknown");
    }

    type PipeQueue = std::sync::Arc<(
        std::sync::Mutex<(std::collections::VecDeque<u8>, bool)>,
        std::sync::Condvar,
    )>;

    struct PipeHalf {
        rx_buf: PipeQueue,
        tx_buf: PipeQueue,
    }

    impl PipeHalf {
        fn pair() -> (Self, Self) {
            let a = std::sync::Arc::new((
                std::sync::Mutex::new((std::collections::VecDeque::new(), false)),
                std::sync::Condvar::new(),
            ));
            let b = std::sync::Arc::new((
                std::sync::Mutex::new((std::collections::VecDeque::new(), false)),
                std::sync::Condvar::new(),
            ));
            (
                Self {
                    rx_buf: a.clone(),
                    tx_buf: b.clone(),
                },
                Self {
                    rx_buf: b,
                    tx_buf: a,
                },
            )
        }
    }

    impl Drop for PipeHalf {
        fn drop(&mut self) {
            let (lock, cv) = &*self.tx_buf;
            if let Ok(mut guard) = lock.lock() {
                guard.1 = true;
                cv.notify_all();
            }
        }
    }

    impl Read for PipeHalf {
        fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
            let (lock, cv) = &*self.rx_buf;
            let mut guard = lock.lock().map_err(|_| std::io::Error::other("poison"))?;
            while guard.0.is_empty() && !guard.1 {
                guard = cv
                    .wait(guard)
                    .map_err(|_| std::io::Error::other("poison"))?;
            }
            let count = guard.0.len().min(out.len());
            for slot in out.iter_mut().take(count) {
                *slot = guard.0.pop_front().unwrap();
            }
            Ok(count)
        }
    }

    impl Write for PipeHalf {
        fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
            let (lock, cv) = &*self.tx_buf;
            let mut guard = lock.lock().map_err(|_| std::io::Error::other("poison"))?;
            guard.0.extend(data);
            cv.notify_all();
            Ok(data.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn guest_lab_session_completes_handshake_lease_heartbeat_and_rejects_bad_replies() {
        let secret = [0x42u8; 32];
        let (mut guest, mut host) = PipeHalf::pair();
        let host_thread = std::thread::spawn(move || {
            let (msg, payload, _) = read_frame(&mut host, MAX_CONTROL_PAYLOAD).unwrap();
            assert_eq!(msg, MSG_HANDSHAKE);
            let hs: Handshake = decode_control(&payload).unwrap();
            let transcript = HandshakeTranscript {
                min_version: hs.min_version,
                max_version: hs.max_version,
                boot_id: hs.boot_id,
                distro_id: hs.distro_id,
                guest_nonce: hs.guest_nonce,
            };
            assert!(verify_guest_proof(&secret, &transcript, &hs.guest_proof));
            let challenge = HostChallenge {
                accepted_version: IPC_VERSION_3,
                heartbeat_secs: 1,
                lease_timeout_secs: 15,
                host_nonce: [7u8; 32],
            };
            let ack = HandshakeAck {
                accepted_version: challenge.accepted_version,
                heartbeat_secs: challenge.heartbeat_secs,
                lease_timeout_secs: challenge.lease_timeout_secs,
                host_nonce: challenge.host_nonce,
                host_proof: host_proof(&secret, &transcript, &challenge).unwrap(),
            };
            write_frame(
                &mut host,
                MSG_HANDSHAKE_ACK,
                &encode_control(&ack).unwrap(),
                1,
            )
            .unwrap();
            let (msg, payload, _) = read_frame(&mut host, MAX_CONTROL_PAYLOAD).unwrap();
            assert_eq!(msg, MSG_HANDSHAKE_FINISH);
            let fin: HandshakeFinish = decode_control(&payload).unwrap();
            assert!(verify_guest_finish_proof(
                &secret,
                &transcript,
                &challenge,
                &fin.guest_finish_proof
            ));
            let (msg, _, _) = read_frame(&mut host, MAX_CONTROL_PAYLOAD).unwrap();
            assert_eq!(msg, MSG_LEASE_REQUEST);
            let granted = LeaseGranted {
                lease_id: 77,
                deadline_ms: 50_000,
            };
            write_frame(
                &mut host,
                MSG_LEASE_GRANTED,
                &encode_control(&granted).unwrap(),
                3,
            )
            .unwrap();
            let (msg, _, _) = read_frame(&mut host, MAX_CONTROL_PAYLOAD).unwrap();
            assert_eq!(msg, MSG_HEARTBEAT);
            let hb_ack = HeartbeatAck {
                timestamp_ms: 40_000,
                lease_remaining_ms: 14_000,
                host_memory: None,
            };
            write_frame(
                &mut host,
                MSG_HEARTBEAT_ACK,
                &encode_control(&hb_ack).unwrap(),
                4,
            )
            .unwrap();
            let (msg, _, _) = read_frame(&mut host, MAX_CONTROL_PAYLOAD).unwrap();
            assert_eq!(msg, MSG_LEASE_RELEASE);
        });

        let (lease_id, deadline, remaining) =
            run_session(&mut guest, &secret, "boot-1".into(), "ubuntu".into()).unwrap();
        assert_eq!(lease_id, 77);
        assert_eq!(deadline, 50_000);
        assert_eq!(remaining, 14_000);
        host_thread.join().unwrap();

        let (mut bad_guest, mut bad_host) = PipeHalf::pair();
        std::thread::spawn(move || {
            let _ = read_frame(&mut bad_host, MAX_CONTROL_PAYLOAD).unwrap();
            write_frame(&mut bad_host, MSG_HEARTBEAT_ACK, b"{}", 1).unwrap();
        });
        assert!(run_session(&mut bad_guest, &secret, "b".into(), "u".into()).is_err());
    }
}
