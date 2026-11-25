// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-winsvc/tests/gpu_identity.rs
//! Native identity producer policy; injected observations never authorize allocation.
#![allow(clippy::unwrap_used, clippy::expect_used)]
use ramshared_vram::GpuAdapterIdentity;
use ramshared_winsvc::gpu_identity::{
    HostGpuIdentityReport, IdentitySource, MAX_IDENTITY_ADAPTERS, MAX_IDENTITY_REPORT_BYTES,
    ProducerProcess, collect_identities, decode_report, encode_report,
};

fn adapter(index: usize) -> GpuAdapterIdentity {
    GpuAdapterIdentity {
        backend: "cuda".into(),
        key: format!("{:032x}", index + 1),
        luid: Some(format!("00000000:{:08x}", index + 1)),
        node_mask: Some(1),
    }
}
struct Source {
    count: i32,
    adapters: Vec<GpuAdapterIdentity>,
    interfaces: Vec<String>,
    calls: Vec<String>,
    fail: Option<&'static str>,
}
impl Source {
    fn new(count: usize) -> Self {
        Self {
            count: count as i32,
            adapters: (0..count).map(adapter).collect(),
            interfaces: (0..count)
                .map(|i| format!(r"\\?\opaque-device-{i}\GPUPARAV"))
                .collect(),
            calls: vec![],
            fail: None,
        }
    }
}
impl IdentitySource for Source {
    fn device_count(&mut self) -> Result<i32, String> {
        self.calls.push("count".into());
        if self.fail == Some("count") {
            Err("native count failure".into())
        } else {
            Ok(self.count)
        }
    }
    fn adapter_identity(&mut self, ordinal: u32) -> Result<GpuAdapterIdentity, String> {
        self.calls.push(format!("identity:{ordinal}"));
        if self.fail == Some("identity") {
            Err("native identity failure".into())
        } else {
            Ok(self.adapters[ordinal as usize].clone())
        }
    }
    fn adapter_interface(&mut self, identity: &GpuAdapterIdentity) -> Result<String, String> {
        self.calls.push("interface".into());
        if self.fail == Some("interface") {
            return Err("native interface failure".into());
        }
        Ok(self.interfaces[self.adapters.iter().position(|a| a == identity).unwrap()].clone())
    }
}
fn report() -> HostGpuIdentityReport {
    HostGpuIdentityReport {
        schema_version: 1,
        producer: ProducerProcess {
            pid: 12,
            creation_time_100ns: 34,
        },
        started_at_unix_ms: 56,
        elapsed_ms: 0,
        adapters: collect_identities(&mut Source::new(2)).unwrap(),
    }
}
#[test]
fn host_identity_collection_keeps_two_independent_native_adapters() {
    let mut source = Source::new(2);
    source.adapters.swap(0, 1);
    let rows = collect_identities(&mut source).unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].ordinal, 0);
    assert_eq!(rows[0].allocator, adapter(1));
    assert_eq!(rows[1].allocator, adapter(0));
    assert_ne!(rows[0].interface_id, rows[1].interface_id);
    assert_eq!(
        source.calls,
        [
            "count",
            "identity:0",
            "interface",
            "identity:1",
            "interface"
        ]
    );
    assert!(collect_identities(&mut Source::new(0)).unwrap().is_empty());
}
#[test]
fn native_identity_count_is_bounded_before_any_adapter_query() {
    for count in [-1, MAX_IDENTITY_ADAPTERS as i32 + 1, i32::MAX] {
        let mut source = Source::new(0);
        source.count = count;
        assert!(collect_identities(&mut source).is_err());
        assert_eq!(source.calls, ["count"]);
    }
    assert_eq!(
        collect_identities(&mut Source::new(MAX_IDENTITY_ADAPTERS))
            .unwrap()
            .len(),
        MAX_IDENTITY_ADAPTERS
    );
}
#[test]
fn host_identity_refuses_missing_or_unproved_allocator_scope_before_interface_query() {
    for change in [
        "backend",
        "uuid",
        "uuid-case",
        "luid",
        "luid-case",
        "node",
        "nodes",
    ] {
        let mut source = Source::new(1);
        match change {
            "backend" => source.adapters[0].backend = "vulkan".into(),
            "uuid" => source.adapters[0].key = "0".repeat(32),
            "uuid-case" => source.adapters[0].key = "A".repeat(32),
            "luid" => source.adapters[0].luid = None,
            "luid-case" => source.adapters[0].luid = Some("00000000:0000000A".into()),
            "node" => source.adapters[0].node_mask = None,
            _ => source.adapters[0].node_mask = Some(3),
        }
        assert!(collect_identities(&mut source).is_err(), "{change}");
        assert_eq!(source.calls, ["count", "identity:0"], "{change}");
    }
}
#[test]
fn duplicate_or_partition_ambiguous_host_identity_refuses_the_complete_report() {
    for change in ["uuid", "luid", "interface", "interface-case", "linked"] {
        let mut source = Source::new(2);
        match change {
            "uuid" => source.adapters[1].key = source.adapters[0].key.clone(),
            "luid" => source.adapters[1].luid = source.adapters[0].luid.clone(),
            "interface" => source.interfaces[1] = source.interfaces[0].clone(),
            "interface-case" => source.interfaces[1] = source.interfaces[0].to_uppercase(),
            _ => {
                source.adapters[1].luid = source.adapters[0].luid.clone();
                source.adapters[1].node_mask = Some(2);
            }
        }
        assert!(collect_identities(&mut source).is_err(), "{change}");
    }
}
#[test]
fn native_identity_errors_stop_later_queries_without_a_partial_success() {
    for fail in ["count", "identity", "interface"] {
        let mut source = Source::new(2);
        source.fail = Some(fail);
        assert!(collect_identities(&mut source).is_err());
        assert!(!source.calls.iter().any(|c| c == "identity:1"));
    }
}
#[test]
fn interface_validation_preserves_opaque_unicode_and_rejects_malformed_bounds() {
    for bad in [
        String::new(),
        "x".repeat(260),
        "x\0y".into(),
        "x\ny".into(),
        "😀".repeat(130),
    ] {
        let mut source = Source::new(1);
        source.interfaces[0] = bad;
        assert!(collect_identities(&mut source).is_err());
    }
    for good in [
        "x".repeat(259),
        "😀".repeat(129),
        r"\\?\厂商-device\GPUPARAV".into(),
    ] {
        let mut source = Source::new(1);
        source.interfaces[0] = good.clone();
        assert_eq!(
            collect_identities(&mut source).unwrap()[0].interface_id,
            good
        );
    }
}
#[test]
fn identity_report_roundtrips_and_revalidates_all_observations() {
    let report = report();
    let encoded = encode_report(&report).unwrap();
    assert_eq!(decode_report(&encoded).unwrap(), report);
    for change in [
        "version",
        "pid",
        "creation",
        "clock",
        "age",
        "ordinal",
        "identity",
        "interface",
        "duplicate",
    ] {
        let mut report = report.clone();
        match change {
            "version" => report.schema_version = 2,
            "pid" => report.producer.pid = 0,
            "creation" => report.producer.creation_time_100ns = 0,
            "clock" => report.started_at_unix_ms = 0,
            "age" => report.elapsed_ms = ramshared_vram::GPU_BUDGET_MAX_AGE_MS,
            "ordinal" => report.adapters[0].ordinal = 1,
            "identity" => report.adapters[0].allocator.node_mask = None,
            "interface" => report.adapters[0].interface_id.clear(),
            _ => report.adapters[1] = report.adapters[0].clone(),
        }
        assert!(encode_report(&report).is_err(), "{change}");
        assert!(
            decode_report(&serde_json::to_vec(&report).unwrap()).is_err(),
            "{change}"
        );
    }
}
#[test]
fn identity_report_refuses_oversized_unknown_truncated_and_excessive_input() {
    assert!(decode_report(&vec![b' '; MAX_IDENTITY_REPORT_BYTES + 1]).is_err());
    assert!(decode_report(b"{}").is_err());
    assert!(decode_report(b"{").is_err());
    let mut value = serde_json::to_value(report()).unwrap();
    value["unexpected"] = true.into();
    assert!(decode_report(&serde_json::to_vec(&value).unwrap()).is_err());
    let mut report = report();
    report.adapters = vec![report.adapters[0].clone(); MAX_IDENTITY_ADAPTERS + 1];
    assert!(encode_report(&report).is_err());
}

#[cfg(not(windows))]
#[test]
fn public_identity_command_refuses_non_windows_without_a_storage_configuration() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_ramshared-winsvc"))
        .args(["inspect-gpus", "--json"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("requires Windows")
    );
}

#[cfg(windows)]
#[test]
#[ignore = "read-only: requires installed native NVIDIA GPU driver; no context or allocation"]
fn public_native_identity_command_matches_its_owned_windows_process() {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::Threading::GetProcessTimes;
    let current = std::env::current_exe().unwrap();
    let executable = current
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("ramshared-winsvc.exe");
    assert!(executable.is_file());
    let child = std::process::Command::new(&executable)
        .args(["inspect-gpus", "--json"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let pid = child.id();
    eprintln!(
        "native inspection parent_pid={} child_pid={pid}",
        std::process::id()
    );
    let mut created = FILETIME::default();
    let mut exited = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    // SAFETY: this exact child remains owned and its handle stays live through wait.
    assert_ne!(
        unsafe {
            GetProcessTimes(
                child.as_raw_handle(),
                &mut created,
                &mut exited,
                &mut kernel,
                &mut user,
            )
        },
        0
    );
    let creation = (u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime);
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    let report = decode_report(&output.stdout).unwrap();
    assert_eq!(report.producer.pid, pid);
    assert_eq!(report.producer.creation_time_100ns, creation);
    assert!(!report.adapters.is_empty());
    assert!(output.stdout.len() <= MAX_IDENTITY_REPORT_BYTES);
    eprintln!(
        "NATIVE_IDENTITY_REPORT {}",
        String::from_utf8(output.stdout).unwrap()
    );
}
