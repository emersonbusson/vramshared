// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-winsvc/src/vm_identity/api.rs
//! Optional ComputeCore entrypoints, loaded only from the native system directory.
use std::ffi::c_void;
use windows_sys::Win32::Foundation::{FreeLibrary, HMODULE};
use windows_sys::Win32::System::HostComputeSystem::{HCS_OPERATION, HCS_OPERATION_COMPLETION};
use windows_sys::Win32::System::LibraryLoader::{
    GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW,
};
use windows_sys::Win32::System::SystemInformation::GetSystemDirectoryW;
type Create = unsafe extern "system" fn(*const c_void, HCS_OPERATION_COMPLETION) -> HCS_OPERATION;
type Enumerate = unsafe extern "system" fn(*const u16, HCS_OPERATION) -> i32;
type Wait = unsafe extern "system" fn(HCS_OPERATION, u32, *mut *mut u16) -> i32;
type Close = unsafe extern "system" fn(HCS_OPERATION);
struct Library(HMODULE);
impl Drop for Library {
    fn drop(&mut self) {
        // SAFETY: this owner holds the unique successful LoadLibraryExW reference.
        let _ = unsafe { FreeLibrary(self.0) };
    }
}
impl Library {
    fn load() -> Result<Self, String> {
        let mut path = vec![0u16; 32768];
        // SAFETY: writable UTF-16 buffer and its actual capacity.
        let length = unsafe { GetSystemDirectoryW(path.as_mut_ptr(), path.len() as u32) } as usize;
        if length == 0 || length >= path.len() {
            return Err(std::io::Error::last_os_error().to_string());
        }
        path.truncate(length);
        path.extend("\\computecore.dll".encode_utf16());
        path.push(0);
        // SAFETY: kernel-provided absolute system directory, fixed module name,
        // system-directory-only dependency search; no caller/CWD/PATH search.
        let module = unsafe {
            LoadLibraryExW(
                path.as_ptr(),
                std::ptr::null_mut(),
                LOAD_LIBRARY_SEARCH_SYSTEM32,
            )
        };
        if module.is_null() {
            return Err(std::io::Error::last_os_error().to_string());
        }
        Ok(Self(module))
    }
    fn symbol(&self, name: &[u8]) -> Result<unsafe extern "system" fn() -> isize, String> {
        // SAFETY: owned live module and fixed NUL-terminated SDK export name.
        unsafe { GetProcAddress(self.0, name.as_ptr()) }
            .ok_or_else(|| "ComputeCore SDK entrypoint unavailable".into())
    }
}
pub(super) struct Api {
    _library: Library,
    pub create: Create,
    pub enumerate: Enumerate,
    pub wait: Wait,
    pub close: Close,
}
impl Api {
    pub fn load() -> Result<Self, String> {
        let library = Library::load()?;
        // SAFETY: each fixed export uses its exact ComputeCore.h system ABI;
        // the library remains owned until all operations borrowing this API close.
        let (create, enumerate, wait, close) = unsafe {
            (
                std::mem::transmute::<unsafe extern "system" fn() -> isize, Create>(
                    library.symbol(b"HcsCreateOperation\0")?,
                ),
                std::mem::transmute::<unsafe extern "system" fn() -> isize, Enumerate>(
                    library.symbol(b"HcsEnumerateComputeSystems\0")?,
                ),
                std::mem::transmute::<unsafe extern "system" fn() -> isize, Wait>(
                    library.symbol(b"HcsWaitForOperationResult\0")?,
                ),
                std::mem::transmute::<unsafe extern "system" fn() -> isize, Close>(
                    library.symbol(b"HcsCloseOperation\0")?,
                ),
            )
        };
        Ok(Self {
            _library: library,
            create,
            enumerate,
            wait,
            close,
        })
    }
}
