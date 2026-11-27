// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-block/src/storage_inventory/parse.rs
//! Bounded Linux inventory decoding; unknown fields remain untrusted data.
use super::{BlockDevice, LINUX_INVENTORY_OUTPUT_LIMIT, MountInfo};
use serde_json::Value;

fn json_string(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_owned)
}

fn json_u64(value: &Value, key: &str) -> Option<u64> {
    let value = value.get(key)?;
    value
        .as_u64()
        .or_else(|| value.as_str().and_then(|text| text.parse().ok()))
}

fn json_bool(value: &Value, key: &str) -> Option<bool> {
    let value = value.get(key)?;
    value
        .as_bool()
        .or_else(|| value.as_u64().map(|number| number != 0))
        .or_else(|| {
            value
                .as_str()
                .and_then(|text| match text.to_ascii_lowercase().as_str() {
                    "true" | "1" => Some(true),
                    "false" | "0" => Some(false),
                    _ => None,
                })
        })
}

fn parse_mountpoints(value: &Value) -> Vec<String> {
    match value.get("mountpoints") {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect(),
        Some(Value::String(item)) if !item.is_empty() => vec![item.clone()],
        _ => json_string(value, "mountpoint").into_iter().collect(),
    }
}

fn hardware_identity(value: &Value) -> Option<String> {
    json_string(value, "wwn")
        .filter(|identity| !identity.trim().is_empty())
        .map(|identity| format!("wwn:{identity}"))
        .or_else(|| {
            json_string(value, "serial")
                .filter(|identity| !identity.trim().is_empty())
                .map(|identity| format!("serial:{identity}"))
        })
}

fn subsystem_local_transport(value: &Value) -> Option<String> {
    let subsystems = json_string(value, "subsystems")?;
    let tokens: Vec<&str> = subsystems.split(':').collect();
    if tokens.iter().any(|t| t.eq_ignore_ascii_case("scsi"))
        && tokens.iter().any(|t| t.eq_ignore_ascii_case("vmbus"))
    {
        return Some("scsi".to_string());
    }
    None
}

fn parse_block_device(
    value: &Value,
    parent_hardware_identity: Option<&str>,
    parent_transport: Option<&str>,
    parent_removable: Option<bool>,
    devices: &mut Vec<BlockDevice>,
) -> Result<(), String> {
    validate_node(value)?;
    let name =
        json_string(value, "name").ok_or_else(|| "lsblk device is missing name".to_string())?;
    let path =
        json_string(value, "path").ok_or_else(|| format!("lsblk device {name} is missing path"))?;
    let kind =
        json_string(value, "type").ok_or_else(|| format!("lsblk device {name} is missing type"))?;
    let filesystem = json_string(value, "fstype");
    let mountpoints = parse_mountpoints(value);
    let read_only = json_bool(value, "ro");
    let removable = match (json_bool(value, "rm"), parent_removable) {
        (Some(true), _) | (_, Some(true)) => Some(true),
        (Some(false), _) | (_, Some(false)) => Some(false),
        (None, None) => None,
    };
    let transport = json_string(value, "tran")
        .or_else(|| subsystem_local_transport(value))
        .or_else(|| parent_transport.map(str::to_owned));
    let hardware_identity = hardware_identity(value);

    devices.push(BlockDevice {
        name,
        path,
        kind,
        size_bytes: json_u64(value, "size"),
        filesystem,
        uuid: json_string(value, "uuid"),
        mountpoints,
        parent: json_string(value, "pkname"),
        major_minor: json_string(value, "maj:min"),
        partition_uuid: json_string(value, "partuuid"),
        hardware_identity: hardware_identity.clone(),
        parent_hardware_identity: parent_hardware_identity.map(str::to_owned),
        mounts: Vec::new(),
        read_only,
        removable,
        rotational: json_bool(value, "rota"),
        transport: transport.clone(),
        model: json_string(value, "model"),
        eligible_for_file_storage: false,
        eligibility_reason: "current mount, capacity, and stable identity are not verified"
            .to_string(),
    });

    if let Some(children) = value.get("children").and_then(Value::as_array) {
        let child_parent_identity = hardware_identity.as_deref().or(parent_hardware_identity);
        let child_transport = transport.as_deref().or(parent_transport);
        let child_removable = removable.or(parent_removable);
        for child in children {
            parse_block_device(
                child,
                child_parent_identity,
                child_transport,
                child_removable,
                devices,
            )?;
        }
    }
    Ok(())
}

pub fn parse_lsblk_json(text: &str) -> Result<Vec<BlockDevice>, String> {
    if text.len() > LINUX_INVENTORY_OUTPUT_LIMIT {
        return Err("lsblk inventory exceeds its byte budget".to_string());
    }
    let root: Value =
        serde_json::from_str(text).map_err(|error| format!("lsblk JSON is invalid: {error}"))?;
    let blockdevices = root
        .get("blockdevices")
        .and_then(Value::as_array)
        .ok_or_else(|| "lsblk JSON has no blockdevices array".to_string())?;
    let mut devices = Vec::new();
    for device in blockdevices {
        parse_block_device(device, None, None, None, &mut devices)?;
    }
    Ok(devices)
}

fn decode_mountinfo_field(field: &str) -> Result<String, String> {
    let bytes = field.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'\\' {
            decoded.push(bytes[index]);
            index += 1;
            continue;
        }
        if index + 3 >= bytes.len()
            || !bytes[index + 1..index + 4]
                .iter()
                .all(|digit| (b'0'..=b'7').contains(digit))
        {
            return Err("mountinfo contains an invalid escape".to_string());
        }
        let value = u16::from(bytes[index + 1] - b'0') * 64
            + u16::from(bytes[index + 2] - b'0') * 8
            + u16::from(bytes[index + 3] - b'0');
        decoded.push(
            u8::try_from(value)
                .map_err(|_| "mountinfo escape is outside the byte range".to_string())?,
        );
        index += 4;
    }
    String::from_utf8(decoded).map_err(|error| format!("mountinfo path is not UTF-8: {error}"))
}

pub fn parse_mountinfo(text: &str) -> Result<Vec<MountInfo>, String> {
    if text.len() > LINUX_INVENTORY_OUTPUT_LIMIT {
        return Err("mount inventory exceeds its byte budget".to_string());
    }
    let mut mounts = Vec::new();
    for (index, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let (mount_fields, filesystem_fields) = line
            .split_once(" - ")
            .ok_or_else(|| format!("mountinfo row {} has no separator", index + 1))?;
        let mount_fields = mount_fields.split_whitespace().collect::<Vec<_>>();
        let filesystem_fields = filesystem_fields.split_whitespace().collect::<Vec<_>>();
        if mount_fields.len() < 6 || filesystem_fields.len() < 3 {
            return Err(format!("mountinfo row {} is malformed", index + 1));
        }
        let mount_id = mount_fields[0]
            .parse::<u64>()
            .map_err(|_| format!("mountinfo row {} has invalid mount ID", index + 1))?;
        let parent_mount_id = mount_fields[1]
            .parse::<u64>()
            .map_err(|_| format!("mountinfo row {} has invalid parent mount ID", index + 1))?;
        let mount_options = mount_fields[5]
            .split(',')
            .map(str::to_owned)
            .collect::<Vec<_>>();
        let super_options = filesystem_fields[2]
            .split(',')
            .map(str::to_owned)
            .collect::<Vec<_>>();
        let read_only = mount_options.iter().any(|option| option == "ro")
            || super_options.iter().any(|option| option == "ro");
        mounts.push(MountInfo {
            mount_id,
            parent_mount_id,
            major_minor: mount_fields[2].to_owned(),
            root: decode_mountinfo_field(mount_fields[3])?,
            mountpoint: decode_mountinfo_field(mount_fields[4])?,
            mount_options,
            filesystem: filesystem_fields[0].to_owned(),
            source: decode_mountinfo_field(filesystem_fields[1])?,
            super_options,
            read_only,
            total_bytes: None,
            available_bytes: None,
            capacity_observed_unix_ms: None,
            host_backing: None,
        });
    }
    Ok(mounts)
}

fn validate_node(value: &Value) -> Result<(), String> {
    use ramshared_config::resource_profile::{MAX_IDENTITY_BYTES, MAX_LINUX_RELATIVE_PATH_BYTES};
    for key in [
        "name",
        "path",
        "type",
        "fstype",
        "uuid",
        "pkname",
        "maj:min",
        "wwn",
        "serial",
        "tran",
        "subsystems",
        "model",
        "partuuid",
    ] {
        let Some(field) = value.get(key).filter(|field| !field.is_null()) else {
            continue;
        };
        let text = field
            .as_str()
            .ok_or_else(|| format!("lsblk {key} is not a string"))?;
        let limit = if key == "path" {
            MAX_LINUX_RELATIVE_PATH_BYTES
        } else {
            MAX_IDENTITY_BYTES
        };
        if text.len() > limit {
            return Err(format!("lsblk {key} exceeds its field budget"));
        }
    }
    if value
        .get("children")
        .is_some_and(|children| !children.is_null() && !children.is_array())
    {
        return Err("lsblk children is not an array".into());
    }
    Ok(())
}
