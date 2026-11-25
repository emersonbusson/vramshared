// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-winsvc/src/installation/root_device/setup_api.rs
//! SetupAPI backend for the owned root device.
//!
//! Split from `root_device.rs` so the decision table stays small enough to
//! review on any host. Every call here runs only on Windows; the parent module
//! owns the refusal and compensation rules.

use super::{
    GuidParts, OWNED_HARDWARE_ID, ObservedDevice, RootBackend, RootDeviceError, RootDevicePlan,
};
use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::ptr;
use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
    DICD_GENERATE_ID, DIF_REGISTERDEVICE, DIGCF_ALLCLASSES, SP_DEVINFO_DATA, SPDRP_HARDWAREID,
    SetupDiCallClassInstaller, SetupDiCreateDeviceInfoList, SetupDiCreateDeviceInfoW,
    SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInfo, SetupDiGetClassDevsW,
    SetupDiGetDeviceInstanceIdW, SetupDiGetDeviceRegistryPropertyW, SetupDiRemoveDevice,
    SetupDiSetDeviceRegistryPropertyW,
};
use windows_sys::Win32::Foundation::{
    ERROR_INSUFFICIENT_BUFFER, ERROR_INVALID_DATA, ERROR_NO_MORE_ITEMS, GetLastError,
    INVALID_HANDLE_VALUE,
};

pub(super) struct SetupApi;

struct DeviceInfoSet(isize);

impl DeviceInfoSet {
    fn all_classes() -> Result<Self, RootDeviceError> {
        let set = unsafe {
            SetupDiGetClassDevsW(ptr::null(), ptr::null(), ptr::null_mut(), DIGCF_ALLCLASSES)
        };
        if set == INVALID_HANDLE_VALUE as isize {
            Err(last_error("SetupDiGetClassDevsW"))
        } else {
            Ok(Self(set))
        }
    }

    fn for_class(class: &windows_sys::core::GUID) -> Result<Self, RootDeviceError> {
        let set = unsafe { SetupDiCreateDeviceInfoList(class, ptr::null_mut()) };
        if set == INVALID_HANDLE_VALUE as isize {
            Err(last_error("SetupDiCreateDeviceInfoList"))
        } else {
            Ok(Self(set))
        }
    }
}

impl Drop for DeviceInfoSet {
    fn drop(&mut self) {
        unsafe {
            SetupDiDestroyDeviceInfoList(self.0);
        }
    }
}

fn last_error(op: &'static str) -> RootDeviceError {
    RootDeviceError::Win32 {
        op,
        code: unsafe { GetLastError() },
    }
}

fn wide(value: &str) -> Vec<u16> {
    OsStr::new(value)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

fn to_guid(parts: &GuidParts) -> windows_sys::core::GUID {
    windows_sys::core::GUID {
        data1: parts.data1,
        data2: parts.data2,
        data3: parts.data3,
        data4: parts.data4,
    }
}

fn instance_id(set: isize, data: &SP_DEVINFO_DATA) -> Result<String, RootDeviceError> {
    let mut size = 256u32;
    loop {
        let mut buffer = vec![0u16; size as usize];
        let mut required = 0u32;
        let ok = unsafe {
            SetupDiGetDeviceInstanceIdW(set, data, buffer.as_mut_ptr(), size, &mut required)
        };
        if ok != 0 {
            let len = buffer
                .iter()
                .position(|unit| *unit == 0)
                .unwrap_or(buffer.len());
            return Ok(String::from_utf16_lossy(&buffer[..len]));
        }
        if unsafe { GetLastError() } == ERROR_INSUFFICIENT_BUFFER && required > size {
            size = required;
            continue;
        }
        return Err(last_error("SetupDiGetDeviceInstanceIdW"));
    }
}

fn hardware_ids(set: isize, data: &SP_DEVINFO_DATA) -> Result<Vec<String>, RootDeviceError> {
    let mut size = 512u32;
    loop {
        let mut buffer = vec![0u8; size as usize];
        let mut required = 0u32;
        let mut kind = 0u32;
        let ok = unsafe {
            SetupDiGetDeviceRegistryPropertyW(
                set,
                data,
                SPDRP_HARDWAREID,
                &mut kind,
                buffer.as_mut_ptr(),
                size,
                &mut required,
            )
        };
        if ok != 0 {
            return Ok(parse_multi_sz(&buffer));
        }
        let code = unsafe { GetLastError() };
        if code == ERROR_INSUFFICIENT_BUFFER {
            size = required.max(size * 2);
            continue;
        }
        if code == ERROR_INVALID_DATA {
            return Ok(Vec::new());
        }
        return Err(RootDeviceError::Win32 {
            op: "SetupDiGetDeviceRegistryPropertyW",
            code,
        });
    }
}

fn parse_multi_sz(bytes: &[u8]) -> Vec<String> {
    let mut values = Vec::new();
    let mut current = Vec::new();
    for chunk in bytes.chunks(2) {
        if chunk.len() != 2 {
            break;
        }
        let unit = u16::from_le_bytes([chunk[0], chunk[1]]);
        if unit == 0 {
            if current.is_empty() {
                break;
            }
            values.push(String::from_utf16_lossy(&current));
            current.clear();
        } else {
            current.push(unit);
        }
    }
    if !current.is_empty() {
        values.push(String::from_utf16_lossy(&current));
    }
    values
}

fn enumerate(set: isize) -> Result<Vec<ObservedDevice>, RootDeviceError> {
    let mut devices = Vec::new();
    let mut index = 0u32;
    loop {
        let mut data = SP_DEVINFO_DATA {
            cbSize: std::mem::size_of::<SP_DEVINFO_DATA>() as u32,
            ..Default::default()
        };
        if unsafe { SetupDiEnumDeviceInfo(set, index, &mut data) } == 0 {
            let code = unsafe { GetLastError() };
            if code == ERROR_NO_MORE_ITEMS {
                return Ok(devices);
            }
            return Err(RootDeviceError::Win32 {
                op: "SetupDiEnumDeviceInfo",
                code,
            });
        }
        devices.push(ObservedDevice {
            instance_id: instance_id(set, &data)?,
            hardware_ids: hardware_ids(set, &data)?,
        });
        index += 1;
    }
}

impl RootBackend for SetupApi {
    fn devices(&mut self) -> Result<Vec<ObservedDevice>, RootDeviceError> {
        let set = DeviceInfoSet::all_classes()?;
        enumerate(set.0)
    }

    fn create(&mut self, plan: &RootDevicePlan) -> Result<String, RootDeviceError> {
        let class = to_guid(&plan.class_guid);
        let set = DeviceInfoSet::for_class(&class)?;
        let name = wide(OWNED_HARDWARE_ID);
        let mut data = SP_DEVINFO_DATA {
            cbSize: std::mem::size_of::<SP_DEVINFO_DATA>() as u32,
            ..Default::default()
        };
        let created = unsafe {
            SetupDiCreateDeviceInfoW(
                set.0,
                name.as_ptr(),
                &class,
                ptr::null(),
                ptr::null_mut(),
                DICD_GENERATE_ID,
                &mut data,
            )
        };
        if created == 0 {
            return Err(last_error("SetupDiCreateDeviceInfoW"));
        }
        let mut multi_sz: Vec<u16> = OWNED_HARDWARE_ID.encode_utf16().collect();
        multi_sz.push(0);
        multi_sz.push(0);
        let property = unsafe {
            SetupDiSetDeviceRegistryPropertyW(
                set.0,
                &mut data,
                SPDRP_HARDWAREID,
                multi_sz.as_ptr().cast::<u8>(),
                (multi_sz.len() * 2) as u32,
            )
        };
        if property == 0 {
            return Err(last_error("SetupDiSetDeviceRegistryPropertyW"));
        }
        if unsafe { SetupDiCallClassInstaller(DIF_REGISTERDEVICE, set.0, &data) } == 0 {
            return Err(last_error("SetupDiCallClassInstaller"));
        }
        instance_id(set.0, &data)
    }

    fn remove(&mut self, target: &str) -> Result<(), RootDeviceError> {
        let set = DeviceInfoSet::all_classes()?;
        let mut index = 0u32;
        loop {
            let mut data = SP_DEVINFO_DATA {
                cbSize: std::mem::size_of::<SP_DEVINFO_DATA>() as u32,
                ..Default::default()
            };
            if unsafe { SetupDiEnumDeviceInfo(set.0, index, &mut data) } == 0 {
                let code = unsafe { GetLastError() };
                if code == ERROR_NO_MORE_ITEMS {
                    return Err(RootDeviceError::Plan(format!(
                        "device {target} is not present"
                    )));
                }
                return Err(RootDeviceError::Win32 {
                    op: "SetupDiEnumDeviceInfo",
                    code,
                });
            }
            if instance_id(set.0, &data)?.eq_ignore_ascii_case(target) {
                let ids = hardware_ids(set.0, &data)?;
                if !ids
                    .iter()
                    .any(|id| id.eq_ignore_ascii_case(OWNED_HARDWARE_ID))
                {
                    return Err(RootDeviceError::Foreign(format!(
                        "{target} does not claim {OWNED_HARDWARE_ID}"
                    )));
                }
                if unsafe { SetupDiRemoveDevice(set.0, &mut data) } == 0 {
                    return Err(last_error("SetupDiRemoveDevice"));
                }
                return Ok(());
            }
            index += 1;
        }
    }
}
