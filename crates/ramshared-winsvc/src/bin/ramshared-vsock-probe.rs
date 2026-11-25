// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-winsvc/src/bin/ramshared-vsock-probe.rs
//! Lab-only raw AF_HYPERV host→guest probe.
//!
//! Connects to a Linux guest's AF_VSOCK listener through the guest VM GUID and
//! sends one `PING`; the guest echo must return `PONG:PING`. This isolates the
//! transport from the lease/protocol code.

use std::io::{Read, Write};
use std::time::Duration;

use ramshared_ipc::vsock::{connect_hyperv, linux_service_guid};

fn parse_guid(value: &str) -> Result<[u8; 16], String> {
    let hex = value.trim().trim_matches(|c| c == '{' || c == '}');
    let parts: Vec<&str> = hex.split('-').collect();
    if parts.len() != 5 || parts[0].len() != 8 || parts[1].len() != 4 || parts[2].len() != 4 {
        return Err(format!("invalid GUID: {value}"));
    }
    if parts[3].len() != 4 || parts[4].len() != 12 {
        return Err(format!("invalid GUID: {value}"));
    }
    let data1 = u32::from_str_radix(parts[0], 16).map_err(|_| "invalid GUID data1")?;
    let data2 = u16::from_str_radix(parts[1], 16).map_err(|_| "invalid GUID data2")?;
    let data3 = u16::from_str_radix(parts[2], 16).map_err(|_| "invalid GUID data3")?;
    let data4a = u16::from_str_radix(parts[3], 16).map_err(|_| "invalid GUID data4")?;
    let data4b = u64::from_str_radix(parts[4], 16).map_err(|_| "invalid GUID data4")?;
    let mut bytes = [0u8; 16];
    bytes[..4].copy_from_slice(&data1.to_be_bytes());
    bytes[4..6].copy_from_slice(&data2.to_be_bytes());
    bytes[6..8].copy_from_slice(&data3.to_be_bytes());
    bytes[8..10].copy_from_slice(&data4a.to_be_bytes());
    bytes[10..16].copy_from_slice(&data4b.to_be_bytes()[2..8]);
    Ok(bytes)
}

#[derive(Debug, PartialEq, Eq)]
struct ProbeArgs {
    vm_id: String,
    vm_guid: [u8; 16],
    port: u32,
    timeout_ms: u64,
}

fn parse_args(args: &[String]) -> Result<ProbeArgs, String> {
    let mut vm_id = None;
    let mut port = None;
    let mut timeout_ms = 5_000u64;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--vm-id" => {
                index += 1;
                vm_id = args.get(index).cloned();
            }
            "--port" => {
                index += 1;
                port = args.get(index).and_then(|value| value.parse::<u32>().ok());
            }
            "--timeout-ms" => {
                index += 1;
                timeout_ms = args
                    .get(index)
                    .and_then(|value| value.parse::<u64>().ok())
                    .ok_or_else(|| "invalid --timeout-ms".to_string())?;
            }
            other => return Err(format!("unknown option: {other}")),
        }
        index += 1;
    }
    let vm_id = vm_id.ok_or_else(|| {
        "usage: ramshared-vsock-probe --vm-id <guid> --port <port> [--timeout-ms N]".to_string()
    })?;
    let port = port.ok_or_else(|| "missing --port".to_string())?;
    if port == 0 || port > 0x7FFF_FFFF {
        return Err("port must be 1..=0x7FFFFFFF".to_string());
    }
    let vm_guid = parse_guid(&vm_id)?;
    Ok(ProbeArgs {
        vm_id,
        vm_guid,
        port,
        timeout_ms: timeout_ms.max(1),
    })
}

fn exchange_ping<S: Read + Write>(stream: &mut S) -> Result<String, Box<dyn std::error::Error>> {
    stream.write_all(b"PING")?;
    stream.flush()?;
    let mut response = [0u8; 64];
    let read = stream.read(&mut response)?;
    let text = String::from_utf8_lossy(&response[..read]).into_owned();
    if text.starts_with("PONG:PING") {
        Ok(text)
    } else {
        Err("guest echo did not match PONG:PING".into())
    }
}

#[cfg(not(test))]
fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let parsed = parse_args(&args)?;
    println!("PROBE_VM_ID={} PORT={}", parsed.vm_id, parsed.port);
    let mut stream = connect_hyperv(
        parsed.vm_guid,
        linux_service_guid(parsed.port),
        Duration::from_millis(parsed.timeout_ms),
    )?;
    println!("PROBE_CONNECT=OK");
    println!("PROBE_SENT=PING");
    match exchange_ping(&mut stream) {
        Ok(text) => {
            println!("PROBE_RECV={text}");
            println!("PROBE_RESULT=PASS");
            Ok(())
        }
        Err(error) => {
            println!("PROBE_RESULT=FAIL");
            Err(error)
        }
    }
}

#[cfg(not(test))]
fn main() {
    if let Err(error) = run() {
        eprintln!("ramshared-vsock-probe: {error}");
        std::process::exit(1);
    }
}

#[cfg(test)]
fn main() {}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use std::io::Cursor;

    struct Duplex {
        written: Vec<u8>,
        reply: Cursor<Vec<u8>>,
    }

    impl Read for Duplex {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.reply.read(buf)
        }
    }

    impl Write for Duplex {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.written.extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn parses_canonical_guid_and_probe_args() {
        let guid = "{d01a61a9-94e3-416a-89eb-f6eb128b179d}";
        let bytes = parse_guid(guid).unwrap();
        assert_eq!(
            bytes,
            [
                0xd0, 0x1a, 0x61, 0xa9, 0x94, 0xe3, 0x41, 0x6a, 0x89, 0xeb, 0xf6, 0xeb, 0x12, 0x8b,
                0x17, 0x9d
            ]
        );
        assert!(parse_guid("not-a-guid").is_err());
        assert!(parse_guid("d01a61a9-94e3-416a-89eb-f6eb128b").is_err());
        assert!(parse_guid("zzzzzzzz-94e3-416a-89eb-f6eb128b179d").is_err());

        let parsed = parse_args(&[
            "--vm-id".into(),
            guid.into(),
            "--port".into(),
            "2761".into(),
            "--timeout-ms".into(),
            "0".into(),
        ])
        .unwrap();
        assert_eq!(parsed.port, 2761);
        assert_eq!(parsed.timeout_ms, 1);
        let _ = Duration::from_millis(parsed.timeout_ms);
        let _ = linux_service_guid(parsed.port);
        let _ = connect_hyperv as fn([u8; 16], [u8; 16], Duration) -> _;

        assert!(parse_args(&[]).is_err());
        assert!(parse_args(&["--vm-id".into(), guid.into()]).is_err());
        assert!(parse_args(&["--vm-id".into(), guid.into(), "--port".into(), "0".into()]).is_err());
        assert!(parse_args(&["--timeout-ms".into(), "nan".into()]).is_err());
        assert!(parse_args(&["--bogus".into()]).is_err());
    }

    #[test]
    fn ping_exchange_accepts_pong_and_rejects_mismatch() {
        let mut ok = Duplex {
            written: Vec::new(),
            reply: Cursor::new(b"PONG:PING\n".to_vec()),
        };
        assert_eq!(exchange_ping(&mut ok).unwrap(), "PONG:PING\n");
        assert_eq!(ok.written, b"PING");

        let mut bad = Duplex {
            written: Vec::new(),
            reply: Cursor::new(b"WRONG".to_vec()),
        };
        assert!(exchange_ping(&mut bad).is_err());
    }
}
