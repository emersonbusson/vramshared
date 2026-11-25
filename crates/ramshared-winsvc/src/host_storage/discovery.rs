// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-winsvc/src/host_storage/discovery.rs
//! Exact-user WSL registration observations. These do not authenticate a guest.

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WslRegistration {
    pub user_sid: String,
    pub registration_guid: String,
    pub distribution_name: String,
    pub vhdx_path: std::path::PathBuf,
}

#[cfg(any(windows, test))]
fn registry_key(sid: &str, guid: &str) -> Option<String> {
    let components: Vec<_> = sid.split('-').collect();
    if sid.len() > 184
        || components.len() < 4
        || components[0] != "S"
        || components[1] != "1"
        || components[2..]
            .iter()
            .any(|part| part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()))
    {
        return None;
    }
    let value = guid.strip_prefix('{')?.strip_suffix('}')?;
    if value.len() != 36
        || value.bytes().all(|byte| byte == b'0' || byte == b'-')
        || !value.bytes().enumerate().all(|(index, byte)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        })
    {
        return None;
    }
    Some(format!(
        r"{sid}\Software\Microsoft\Windows\CurrentVersion\Lxss\{guid}"
    ))
}

#[cfg(any(windows, test))]
const MAX_REGISTRATIONS: usize = 256;

#[cfg(any(windows, test))]
#[derive(Clone, Debug, Eq, PartialEq)]
struct RegistrationKey {
    guid: String,
    last_write: u64,
}

#[cfg(any(windows, test))]
fn validate_keys(sid: &str, keys: &[RegistrationKey]) -> bool {
    keys.len() <= MAX_REGISTRATIONS
        && keys.iter().enumerate().all(|(index, key)| {
            registry_key(sid, &key.guid).is_some()
                && !keys[..index]
                    .iter()
                    .any(|prior| prior.guid.eq_ignore_ascii_case(&key.guid))
        })
}

/// A point-in-time view of one loaded user's registrations. It is not proof
/// of global completeness, guest ownership or durable backing capacity.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UserRegistrationCatalog {
    pub observed_at_unix_ms: u64,
    pub registrations: Vec<WslRegistration>,
}

/// Match one candidate only after every registration's VHD identifier was
/// observed. This is scoped to the supplied catalog and is not guest authority.
pub fn match_registration(
    catalog: &UserRegistrationCatalog,
    root_id: [u8; 16],
    mut observe: impl FnMut(&std::path::Path) -> Option<[u8; 16]>,
) -> Option<&WslRegistration> {
    if root_id == [0; 16]
        || catalog.observed_at_unix_ms == 0
        || catalog.registrations.is_empty()
        || catalog.registrations.len() > 256
    {
        return None;
    }
    let mut seen = std::collections::HashSet::new();
    let mut selected = None;
    for registration in &catalog.registrations {
        let id = observe(&registration.vhdx_path)?;
        if id == [0; 16] || !seen.insert(id) {
            return None;
        }
        if id == root_id {
            selected = Some(registration);
        }
    }
    selected
}

/// Reobserve the same user registration set around all VHD queries. The
/// result remains an observation and must not authorize a guest or swap.
#[cfg(windows)]
pub fn observe_matching_user_registration(
    user_sid: &str,
    root_id: [u8; 16],
) -> Option<WslRegistration> {
    let before = observe_user_catalog(user_sid)?;
    let selected =
        match_registration(&before, root_id, super::vhd::observe_vhd_identifier)?.clone();
    let after = observe_user_catalog(user_sid)?;
    if before.registrations != after.registrations {
        return None;
    }
    Some(selected)
}

/// Observe a bounded user hive twice and refuse registry changes or any
/// unreadable entry. Call only from a supervised child with bounded custody.
#[cfg(windows)]
pub fn observe_user_catalog(user_sid: &str) -> Option<UserRegistrationCatalog> {
    use std::os::windows::ffi::OsStrExt;
    use std::time::{SystemTime, UNIX_EPOCH};
    use windows_sys::Win32::Foundation::{ERROR_NO_MORE_ITEMS, FILETIME};
    use windows_sys::Win32::System::Registry::{
        HKEY, HKEY_USERS, KEY_READ, RegCloseKey, RegEnumKeyExW, RegOpenKeyExW,
    };
    struct Key(HKEY);
    impl Drop for Key {
        fn drop(&mut self) {
            // SAFETY: this handle was opened here and is closed once.
            unsafe {
                RegCloseKey(self.0);
            }
        }
    }
    fn enumerate(key: HKEY, sid: &str) -> Option<Vec<RegistrationKey>> {
        let mut result = Vec::new();
        for index in 0..=MAX_REGISTRATIONS {
            let mut name = [0u16; 128];
            let mut size = name.len() as u32;
            let mut time = FILETIME::default();
            // SAFETY: handle is open and all supplied output buffers are live.
            let status = unsafe {
                RegEnumKeyExW(
                    key,
                    index as u32,
                    name.as_mut_ptr(),
                    &mut size,
                    std::ptr::null(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    &mut time,
                )
            };
            if status == ERROR_NO_MORE_ITEMS {
                if !validate_keys(sid, &result) {
                    return None;
                }
                result.sort_by_key(|item| item.guid.to_ascii_lowercase());
                return Some(result);
            }
            if status != 0 || index == MAX_REGISTRATIONS || size == 0 || size as usize >= name.len()
            {
                return None;
            }
            let guid = String::from_utf16(&name[..size as usize]).ok()?;
            let last_write = (u64::from(time.dwHighDateTime) << 32) | u64::from(time.dwLowDateTime);
            result.push(RegistrationKey { guid, last_write });
        }
        None
    }
    let observed_at_unix_ms = u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()?
            .as_millis(),
    )
    .ok()?;
    if observed_at_unix_ms == 0 {
        return None;
    }
    // Reuse the exact SID validation without accepting a caller-provided path.
    registry_key(user_sid, "{00000001-0000-0000-0000-000000000000}")?;
    let path = format!(r"{user_sid}\Software\Microsoft\Windows\CurrentVersion\Lxss");
    let wide: Vec<u16> = std::ffi::OsStr::new(&path)
        .encode_wide()
        .chain(Some(0))
        .collect();
    let mut raw: HKEY = std::ptr::null_mut();
    // SAFETY: a terminated fixed-root path and one live output handle slot.
    if unsafe { RegOpenKeyExW(HKEY_USERS, wide.as_ptr(), 0, KEY_READ, &mut raw) } != 0 {
        return None;
    }
    let key = Key(raw);
    let before = enumerate(key.0, user_sid)?;
    let mut registrations = Vec::with_capacity(before.len());
    for item in &before {
        registrations.push(observe_exact(user_sid, &item.guid)?);
    }
    let after = enumerate(key.0, user_sid)?;
    if before != after {
        return None;
    }
    Some(UserRegistrationCatalog {
        observed_at_unix_ms,
        registrations,
    })
}

/// Read one exact registration from a loaded user hive. The caller must still
/// prove that this SID and registration belong to the authenticated guest.
/// Run only under bounded child custody: registry and filesystem calls may hang.
#[cfg(windows)]
pub fn observe_exact(user_sid: &str, registration_guid: &str) -> Option<WslRegistration> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::System::Registry::{
        HKEY, HKEY_USERS, KEY_READ, REG_SZ, RegCloseKey, RegOpenKeyExW, RegQueryValueExW,
    };

    struct Key(HKEY);
    impl Drop for Key {
        fn drop(&mut self) {
            // SAFETY: the key was opened by RegOpenKeyExW and is closed once.
            unsafe {
                RegCloseKey(self.0);
            }
        }
    }
    fn value(key: HKEY, name: &str) -> Option<String> {
        let name: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
        let mut ty = 0;
        let mut bytes = 0u32;
        // SAFETY: query length with live output pointers and a terminated name.
        if unsafe {
            RegQueryValueExW(
                key,
                name.as_ptr(),
                std::ptr::null(),
                &mut ty,
                std::ptr::null_mut(),
                &mut bytes,
            )
        } != 0
            || ty != REG_SZ
            || !(2..=16 * 1024).contains(&bytes)
            || !bytes.is_multiple_of(2)
        {
            return None;
        }
        let mut units = vec![0u16; (bytes / 2) as usize];
        let capacity = bytes;
        // SAFETY: the buffer covers capacity bytes; the API updates bytes.
        if unsafe {
            RegQueryValueExW(
                key,
                name.as_ptr(),
                std::ptr::null(),
                &mut ty,
                units.as_mut_ptr().cast(),
                &mut bytes,
            )
        } != 0
            || ty != REG_SZ
            || bytes != capacity
            || units.last() != Some(&0)
        {
            return None;
        }
        units.pop();
        if units.contains(&0) {
            return None;
        }
        let result = String::from_utf16(&units).ok()?;
        (!result.is_empty() && !result.chars().any(char::is_control)).then_some(result)
    }
    let key = registry_key(user_sid, registration_guid)?;
    let wide: Vec<u16> = std::ffi::OsStr::new(&key)
        .encode_wide()
        .chain(Some(0))
        .collect();
    let mut handle: HKEY = std::ptr::null_mut();
    // SAFETY: name is terminated and output points to one unowned key slot.
    if unsafe { RegOpenKeyExW(HKEY_USERS, wide.as_ptr(), 0, KEY_READ, &mut handle) } != 0 {
        return None;
    }
    let key = Key(handle);
    let distribution_name = value(key.0, "DistributionName")?;
    let base = std::path::PathBuf::from(value(key.0, "BasePath")?);
    if !base.is_absolute()
        || base
            .components()
            .any(|part| matches!(part, std::path::Component::ParentDir))
    {
        return None;
    }
    Some(WslRegistration {
        user_sid: user_sid.into(),
        registration_guid: registration_guid.into(),
        distribution_name,
        vhdx_path: base.join("ext4.vhdx"),
    })
}

mod user_hives;
#[cfg(any(windows, test))]
pub(super) use user_hives::RegistrationObservation;
pub use user_hives::enumerate_all_vhdx_paths;
#[cfg(windows)]
pub(super) use user_hives::stable_loaded_registrations;

#[cfg(test)]
#[path = "discovery/tests.rs"]
#[allow(clippy::unwrap_used)]
mod tests;
