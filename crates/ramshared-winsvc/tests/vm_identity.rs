// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-winsvc/tests/vm_identity.rs
//! Native-property routing is not guest or GPU allocation authority.
#![allow(clippy::unwrap_used, clippy::expect_used)]
use ramshared_ipc::vsock::{VsockPeerIdentity, linux_service_guid};
use ramshared_winsvc::gpu_identity::ProducerProcess;
use ramshared_winsvc::vm_id_resolve::{
    parse_guid, select_connect_candidate, select_connect_candidates, select_guest_vm_guid,
    verify_connected_route_peer,
};
use ramshared_winsvc::vm_identity::{
    VmIdentity, VmIdentityReport, VmOsClassification, project_systems,
};
const ID: &str = "12345678-abcd-4321-8765-123456789abc";
fn row(id: &str, kind: &str, os: &str, state: &str) -> String {
    format!(
        r#"{{"Id":"system-{id}","RuntimeId":"{id}","SystemType":"{kind}","RuntimeOsType":"{os}","State":"{state}","Stopped":false,"Name":"arbitrary","Owner":"untrusted-label"}}"#
    )
}
fn report() -> VmIdentityReport {
    VmIdentityReport {
        schema_version: 1,
        producer: ProducerProcess {
            pid: 42,
            creation_time_100ns: 77,
        },
        started_at_unix_ms: 1000,
        elapsed_ms: 1,
        systems: project_systems(
            format!("[{}]", row(ID, "VirtualMachine", "Linux", "Running")).as_bytes(),
        )
        .unwrap(),
    }
}
#[test]
fn vm_report_projects_native_runtime_without_name_heuristics() {
    let systems = project_systems(
        format!(
            "[{},{}]",
            row(ID, "VirtualMachine", "Linux", "Running"),
            row(
                "00000000-0000-0000-0000-000000000001",
                "Container",
                "Linux",
                "Running"
            )
        )
        .as_bytes(),
    )
    .unwrap();
    assert_eq!(systems.len(), 1);
    assert_eq!(systems[0].runtime_id, parse_guid(ID).unwrap());
    assert_eq!(
        select_guest_vm_guid(&report()).unwrap(),
        parse_guid(ID).unwrap()
    );
    for (kind, os, state) in [
        ("VirtualMachine", "Windows", "Running"),
        ("Container", "Linux", "Running"),
        ("VirtualMachine", "Linux", "Stopped"),
    ] {
        let mut json: serde_json::Value = serde_json::from_str(&row(ID, kind, os, state)).unwrap();
        json["Name"] = "WSL Linux nix".into();
        assert!(
            project_systems(serde_json::to_vec(&vec![json]).unwrap().as_slice())
                .unwrap()
                .is_empty()
        );
    }
}
#[test]
fn vm_report_refuses_ambiguous_missing_malformed_and_excess_data() {
    let mut r = report();
    r.systems.clear();
    assert!(select_guest_vm_guid(&r).is_err());
    r = report();
    r.systems.push(r.systems[0].clone());
    assert!(r.validate().is_err());
    for value in [
        "00000000-0000-0000-0000-000000000000",
        "{{12345678-abcd-4321-8765-123456789abc}}",
        "+2345678-abcd-4321-8765-123456789abc",
    ] {
        assert!(parse_guid(value).is_err(), "{value}");
    }
    let valid = row(ID, "VirtualMachine", "Linux", "Running");
    assert!(project_systems(format!("[{valid},{valid}]").as_bytes()).is_err());
    assert!(project_systems(&vec![b' '; 65537]).is_err());
    assert!(project_systems(b"not JSON").is_err());
    assert!(
        project_systems(
            br#"[{"SystemType":"VirtualMachine","RuntimeOsType":"Linux","State":"Running"}]"#
        )
        .is_err()
    );
    let mut old = report();
    old.elapsed_ms = 5000;
    assert!(old.validate().is_err());
}

#[test]
fn vm_native_projection_bounds_and_required_fields_are_enforced() {
    let valid: serde_json::Value =
        serde_json::from_str(&row(ID, "VirtualMachine", "Linux", "Running")).unwrap();
    for field in ["SystemType", "RuntimeId", "Id"] {
        let mut missing = valid.clone();
        missing.as_object_mut().unwrap().remove(field);
        assert!(
            project_systems(&serde_json::to_vec(&vec![missing]).unwrap()).is_err(),
            "{field}"
        );
    }
    for (field, value) in [
        ("Stopped", serde_json::json!(true)),
        ("RuntimeOsType", serde_json::json!("")),
        ("RuntimeOsType", serde_json::json!("   ")),
        ("Id", serde_json::json!("")),
        ("Id", serde_json::json!("bad\nidentity")),
        ("Id", serde_json::json!("x".repeat(257))),
        (
            "RuntimeId",
            serde_json::json!("00000000-0000-0000-0000-000000000000"),
        ),
    ] {
        let mut invalid = valid.clone();
        invalid[field] = value;
        assert!(project_systems(&serde_json::to_vec(&vec![invalid]).unwrap()).is_err());
    }
    assert!(project_systems(&serde_json::to_vec(&vec![valid.clone(); 65]).unwrap()).is_err());
    let mut distinct_runtime = valid.clone();
    distinct_runtime["RuntimeId"] = "11111111-2222-3333-4444-555555555555".into();
    assert!(project_systems(&serde_json::to_vec(&vec![valid, distinct_runtime]).unwrap()).is_err());
}

#[test]
fn vm_typed_report_refuses_forged_geometry_and_unknown_fields() {
    use ramshared_winsvc::vm_identity::{MAX_VM_REPORT_BYTES, decode_report};
    let valid = report();
    assert_eq!(
        decode_report(&serde_json::to_vec(&valid).unwrap()).unwrap(),
        valid
    );
    for field in ["schema_version", "started_at_unix_ms"] {
        let mut invalid = serde_json::to_value(&valid).unwrap();
        invalid[field] = 0.into();
        assert!(decode_report(&serde_json::to_vec(&invalid).unwrap()).is_err());
    }
    for field in ["pid", "creation_time_100ns"] {
        let mut invalid = valid.clone();
        if field == "pid" {
            invalid.producer.pid = 0;
        } else {
            invalid.producer.creation_time_100ns = 0;
        }
        assert!(decode_report(&serde_json::to_vec(&invalid).unwrap()).is_err());
    }
    let mut unknown = serde_json::to_value(&valid).unwrap();
    unknown["grant_authority"] = true.into();
    assert!(decode_report(&serde_json::to_vec(&unknown).unwrap()).is_err());
    let mut missing_os = serde_json::to_value(&valid).unwrap();
    missing_os["systems"][0]
        .as_object_mut()
        .unwrap()
        .remove("os_classification");
    assert!(decode_report(&serde_json::to_vec(&missing_os).unwrap()).is_err());
    assert!(decode_report(&vec![b' '; MAX_VM_REPORT_BYTES + 1]).is_err());
    let mut excessive = valid.clone();
    excessive.systems = vec![valid.systems[0].clone(); 65];
    assert!(excessive.validate().is_err());
    let mut zero = valid.clone();
    zero.systems[0].runtime_id = [0; 16];
    assert!(zero.validate().is_err());
    let mut ambiguous = valid;
    ambiguous.systems.push(VmIdentity {
        system_id: "other-system".into(),
        runtime_id: [1; 16],
        os_classification: VmOsClassification::Linux,
    });
    assert!(ambiguous.validate().is_ok());
    assert!(select_guest_vm_guid(&ambiguous).is_err());
}

#[test]
fn vm_guid_parser_accepts_canonical_braces_and_refuses_malformed_input() {
    assert_eq!(
        parse_guid(ID).unwrap(),
        parse_guid(&format!(" {{{ID}}} ")).unwrap()
    );
    assert_eq!(
        parse_guid(ID).unwrap(),
        parse_guid(&ID.to_ascii_uppercase()).unwrap()
    );
    for invalid in [
        "",
        "1-2-3-4-5",
        "12345678-abcd-4321-8765-123456789ab",
        "12345678-abcd-4321-8765-123456789abc}",
        "{12345678-abcd-4321-8765-123456789abc",
        "gggggggg-abcd-4321-8765-123456789abc",
    ] {
        assert!(parse_guid(invalid).is_err());
    }
}

#[cfg(not(windows))]
#[test]
fn native_vm_command_refuses_a_non_windows_host() {
    assert_eq!(ramshared_winsvc::vm_identity::inspect_command(), 2);
}

#[test]
fn hcs_created_vm_with_omitted_state_does_not_block_running_wsl() {
    let created = serde_json::json!({
        "Id": "11111111-2222-3333-4444-555555555555",
        "RuntimeId": "11111111-2222-3333-4444-555555555555",
        "SystemType": "VirtualMachine",
        "Owner": "VMMS"
    });
    let running = serde_json::json!({
        "Id": ID, "RuntimeId": ID, "SystemType": "VirtualMachine", "State": "Running"
    });
    let observation = serde_json::to_vec(&[created.clone(), running]).unwrap();
    let systems = project_systems(&observation).unwrap();
    assert_eq!(systems.len(), 1);
    assert_eq!(systems[0].runtime_id, parse_guid(ID).unwrap());
    assert_eq!(systems[0].os_classification, VmOsClassification::Omitted);
    assert!(
        project_systems(&serde_json::to_vec(&[created]).unwrap())
            .unwrap()
            .is_empty()
    );
}

#[test]
fn stopped_vm_without_os_does_not_hide_an_identified_running_guest() {
    let running: serde_json::Value =
        serde_json::from_str(&row(ID, "VirtualMachine", "Linux", "Running")).unwrap();
    let stopped =
        serde_json::json!({"SystemType": "VirtualMachine", "State": "Stopped", "Stopped": true});
    let systems = project_systems(&serde_json::to_vec(&vec![running, stopped]).unwrap()).unwrap();
    assert_eq!(systems.len(), 1);
    assert_eq!(systems[0].runtime_id, parse_guid(ID).unwrap());
    let unknown_running =
        serde_json::json!({"SystemType": "VirtualMachine", "State": "Running", "Stopped": false});
    assert!(project_systems(&serde_json::to_vec(&vec![unknown_running]).unwrap()).is_err());
    let conflicting = serde_json::json!({"SystemType": "VirtualMachine", "State": "Running", "Stopped": true, "RuntimeOsType": "Windows"});
    assert!(project_systems(&serde_json::to_vec(&vec![conflicting]).unwrap()).is_err());
}

#[test]
fn omitted_os_vm_requires_endpoint_proof_and_never_trusts_owner_or_ambiguity() {
    let wsl_like = serde_json::json!({
        "Id": "FDA22A19-A1C0-4A30-B93E-A9F2576C2A59",
        "SystemType": "VirtualMachine",
        "Owner": "WSL",
        "RuntimeId": "fda22a19-a1c0-4a30-b93e-a9f2576c2a59",
        "State": "Running"
    });
    let windows_vm = serde_json::json!({
        "Id": "win-vm",
        "SystemType": "VirtualMachine",
        "RuntimeOsType": "Windows",
        "RuntimeId": "22222222-3333-4444-5555-666666666666",
        "State": "Running"
    });
    let systems =
        project_systems(&serde_json::to_vec(&vec![wsl_like.clone(), windows_vm]).unwrap()).unwrap();
    assert_eq!(systems.len(), 1);
    assert_eq!(systems[0].os_classification, VmOsClassification::Omitted);
    let expected_guid = parse_guid("fda22a19-a1c0-4a30-b93e-a9f2576c2a59").unwrap();
    assert_eq!(systems[0].runtime_id, expected_guid);

    let omitted_report = VmIdentityReport {
        schema_version: 1,
        producer: ProducerProcess {
            pid: 42,
            creation_time_100ns: 77,
        },
        started_at_unix_ms: 1000,
        elapsed_ms: 1,
        systems,
    };
    // Absent OS alone must NOT satisfy the explicit-Linux selector.
    assert!(select_guest_vm_guid(&omitted_report).is_err());

    // Endpoint-verified connect route accepts the unique candidate and requires kernel peer proof.
    let candidate = select_connect_candidate(&omitted_report).unwrap();
    assert_eq!(candidate.runtime_id, expected_guid);
    assert!(candidate.requires_native_peer_proof());

    let valid_peer = VsockPeerIdentity {
        vm_id: expected_guid,
        service_id: linux_service_guid(2761),
    };
    assert_eq!(
        verify_connected_route_peer(candidate.runtime_id, 2761, Some(valid_peer)).unwrap(),
        valid_peer
    );
    // Unscoped stream, wrong VM GUID, wrong service port, and invalid ports must be refused.
    assert!(verify_connected_route_peer(candidate.runtime_id, 2761, None).is_err());
    assert!(
        verify_connected_route_peer(
            candidate.runtime_id,
            2761,
            Some(VsockPeerIdentity {
                vm_id: [9; 16],
                service_id: linux_service_guid(2761),
            }),
        )
        .is_err()
    );
    assert!(
        verify_connected_route_peer(
            candidate.runtime_id,
            2761,
            Some(VsockPeerIdentity {
                vm_id: expected_guid,
                service_id: linux_service_guid(2762),
            }),
        )
        .is_err()
    );
    assert!(verify_connected_route_peer(candidate.runtime_id, 0, Some(valid_peer)).is_err());
    assert!(
        verify_connected_route_peer(candidate.runtime_id, 0x8000_0000, Some(valid_peer)).is_err()
    );

    // Multiple candidates (two omitted or one explicit Linux + one omitted) refuse regardless of Owner="WSL".
    let other_omitted = serde_json::json!({
        "Id": "other-vm",
        "SystemType": "VirtualMachine",
        "Owner": "OtherHost",
        "RuntimeId": "11111111-2222-3333-4444-555555555555",
        "State": "Running"
    });
    let two_omitted = VmIdentityReport {
        systems: project_systems(
            &serde_json::to_vec(&vec![wsl_like.clone(), other_omitted]).unwrap(),
        )
        .unwrap(),
        ..omitted_report.clone()
    };
    assert!(select_connect_candidate(&two_omitted).is_err());

    let explicit_linux: serde_json::Value =
        serde_json::from_str(&row(ID, "VirtualMachine", "Linux", "Running")).unwrap();
    let mixed = VmIdentityReport {
        systems: project_systems(&serde_json::to_vec(&vec![wsl_like, explicit_linux]).unwrap())
            .unwrap(),
        ..omitted_report.clone()
    };
    assert!(select_connect_candidate(&mixed).is_err());
    assert!(select_guest_vm_guid(&mixed).is_err());

    // Empty report refuses.
    let empty = VmIdentityReport {
        systems: Vec::new(),
        ..omitted_report
    };
    assert!(select_connect_candidate(&empty).is_err());

    // Explicit Linux candidate does not omit OS classification.
    let linux_candidate = select_connect_candidate(&report()).unwrap();
    assert!(!linux_candidate.requires_native_peer_proof());
    assert_eq!(linux_candidate.runtime_id, parse_guid(ID).unwrap());
}

#[test]
fn select_connect_candidates_orders_linux_before_omitted_and_refuses_empty() {
    let wsl_like = serde_json::json!({
        "Id": "FDA22A19-A1C0-4A30-B93E-A9F2576C2A59",
        "SystemType": "VirtualMachine",
        "Owner": "WSL",
        "RuntimeId": "fda22a19-a1c0-4a30-b93e-a9f2576c2a59",
        "State": "Running"
    });
    let other_omitted = serde_json::json!({
        "Id": "other-hyperv-vm",
        "SystemType": "VirtualMachine",
        "Owner": "Microsoft.Virtualization",
        "RuntimeId": "11111111-2222-3333-4444-555555555555",
        "State": "Running"
    });
    let explicit_linux: serde_json::Value =
        serde_json::from_str(&row(ID, "VirtualMachine", "Linux", "Running")).unwrap();
    let multi = VmIdentityReport {
        schema_version: 1,
        producer: ProducerProcess {
            pid: 42,
            creation_time_100ns: 77,
        },
        started_at_unix_ms: 1000,
        elapsed_ms: 1,
        systems: project_systems(
            &serde_json::to_vec(&vec![wsl_like, other_omitted, explicit_linux]).unwrap(),
        )
        .unwrap(),
    };
    let candidates = select_connect_candidates(&multi).unwrap();
    assert_eq!(candidates.len(), 3);
    assert_eq!(candidates[0].os_classification, VmOsClassification::Linux);
    assert_eq!(candidates[0].runtime_id, parse_guid(ID).unwrap());
    assert_eq!(candidates[1].os_classification, VmOsClassification::Omitted);
    assert_eq!(
        candidates[1].runtime_id,
        parse_guid("fda22a19-a1c0-4a30-b93e-a9f2576c2a59").unwrap()
    );
    assert_eq!(candidates[2].os_classification, VmOsClassification::Omitted);
    assert_eq!(
        candidates[2].runtime_id,
        parse_guid("11111111-2222-3333-4444-555555555555").unwrap()
    );

    let mut empty = multi;
    empty.systems.clear();
    assert!(select_connect_candidates(&empty).is_err());
}
