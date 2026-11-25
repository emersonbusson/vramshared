// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-winsvc/src/gpu_identity/process/spawn.rs
//! Atomic private-job association and explicit inherited stdio handles.
use super::{NativeProcess, check};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::Path;
use windows_sys::Win32::Foundation::{GENERIC_READ, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows_sys::Win32::System::JobObjects::{
    CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JobObjectExtendedLimitInformation, SetInformationJobObject,
};
use windows_sys::Win32::System::Threading::{
    CREATE_NO_WINDOW, CreateProcessW, DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT,
    InitializeProcThreadAttributeList, LPPROC_THREAD_ATTRIBUTE_LIST,
    PROC_THREAD_ATTRIBUTE_HANDLE_LIST, PROC_THREAD_ATTRIBUTE_JOB_LIST, PROCESS_INFORMATION,
    STARTF_USESTDHANDLES, STARTUPINFOEXW, STARTUPINFOW, UpdateProcThreadAttribute,
};

const _: () = assert!(std::mem::size_of::<SECURITY_ATTRIBUTES>() == 24);
const _: () = assert!(std::mem::size_of::<STARTUPINFOW>() == 104);
const _: () = assert!(std::mem::size_of::<STARTUPINFOEXW>() == 112);
const _: () = assert!(std::mem::size_of::<PROCESS_INFORMATION>() == 24);
const _: () = assert!(std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() == 144);

pub(super) fn own(handle: HANDLE) -> Result<OwnedHandle, String> {
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        return Err(super::last_error());
    }
    // SAFETY: successful creator transfers this unique non-pseudo handle once.
    Ok(unsafe { OwnedHandle::from_raw_handle(handle) })
}
pub(super) fn attributes() -> SECURITY_ATTRIBUTES {
    SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: std::ptr::null_mut(),
        bInheritHandle: 1,
    }
}
fn job() -> Result<OwnedHandle, String> {
    // SAFETY: unnamed private job, default non-inheritable security attributes.
    let job = own(unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) })?;
    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    // SAFETY: owned job, exact live SDK layout/class. No breakaway flags.
    check(unsafe {
        SetInformationJobObject(
            job.as_raw_handle(),
            JobObjectExtendedLimitInformation,
            (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
            std::mem::size_of_val(&limits) as u32,
        )
    })?;
    Ok(job)
}
struct AttributeList {
    storage: Vec<usize>,
}
impl AttributeList {
    fn pointer(&mut self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
        self.storage.as_mut_ptr().cast()
    }
    fn new() -> Result<Self, String> {
        let mut bytes = 0;
        // SAFETY: documented size query with null storage and live size output.
        unsafe {
            InitializeProcThreadAttributeList(std::ptr::null_mut(), 2, 0, &mut bytes);
        }
        if bytes == 0 || bytes > 64 * 1024 {
            return Err("native attribute-list size is invalid".into());
        }
        let words = bytes.div_ceil(std::mem::size_of::<usize>());
        let mut storage = vec![0_usize; words];
        // SAFETY: pointer-aligned stable allocation covering the queried bytes.
        // Initialize before constructing the RAII guard that invokes Delete.
        check(unsafe {
            InitializeProcThreadAttributeList(storage.as_mut_ptr().cast(), 2, 0, &mut bytes)
        })?;
        Ok(Self { storage })
    }
    fn set(&mut self, attribute: u32, handles: &[HANDLE]) -> Result<(), String> {
        // SAFETY: live initialized list and stable handle array retained through
        // CreateProcessW. Values are JOB_LIST or explicit HANDLE_LIST only.
        check(unsafe {
            UpdateProcThreadAttribute(
                self.pointer(),
                0,
                attribute as usize,
                handles.as_ptr().cast(),
                std::mem::size_of_val(handles),
                std::ptr::null_mut(),
                std::ptr::null(),
            )
        })
    }
}
impl Drop for AttributeList {
    fn drop(&mut self) {
        // SAFETY: initialized list's allocation is live until after Delete.
        unsafe {
            DeleteProcThreadAttributeList(self.pointer());
        }
    }
}
pub(super) fn spawn(executable: &Path, arguments: &str) -> Result<NativeProcess, String> {
    let exe: Vec<u16> = executable.as_os_str().encode_wide().collect();
    if !executable.is_absolute()
        || exe.is_empty()
        || exe.len() > 32767
        || exe.contains(&0)
        || exe.contains(&(b'"' as u16))
    {
        return Err("native producer requires a bounded absolute executable path".into());
    }
    let mut command: Vec<u16> = [34]
        .into_iter()
        .chain(exe.iter().copied())
        .chain([34, 32])
        .chain(arguments.encode_utf16())
        .collect();
    if command.len() >= 32767 || command.contains(&0) {
        return Err("native producer command exceeds Windows bound or contains NUL".into());
    }
    command.push(0);
    let exe: Vec<u16> = exe.into_iter().chain([0]).collect();
    let job = job()?;
    let (stdout, stdout_write) = super::pipe::create()?;
    let (stderr, stderr_write) = super::pipe::create()?;
    let nul: Vec<u16> = "NUL\0".encode_utf16().collect();
    // SAFETY: terminated literal, live inheritable security record; own exact
    // NUL read handle. The command never inherits the serving process stdin.
    let stdin = own(unsafe {
        CreateFileW(
            nul.as_ptr(),
            GENERIC_READ,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            &attributes(),
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            std::ptr::null_mut(),
        )
    })?;
    let jobs = [job.as_raw_handle()];
    let inherited = [
        stdin.as_raw_handle(),
        stdout_write.as_raw_handle(),
        stderr_write.as_raw_handle(),
    ];
    let mut attributes = AttributeList::new()?;
    attributes.set(PROC_THREAD_ATTRIBUTE_JOB_LIST, &jobs)?;
    attributes.set(PROC_THREAD_ATTRIBUTE_HANDLE_LIST, &inherited)?;
    let mut startup = STARTUPINFOEXW::default();
    startup.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
    startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    startup.StartupInfo.hStdInput = stdin.as_raw_handle();
    startup.StartupInfo.hStdOutput = stdout_write.as_raw_handle();
    startup.StartupInfo.hStdError = stderr_write.as_raw_handle();
    startup.lpAttributeList = attributes.pointer();
    let mut info = PROCESS_INFORMATION::default();
    // SAFETY: exact extended SDK layout, bounded writable terminated command,
    // explicit absolute application and stable attribute/handle arrays. Job
    // association is atomic with creation; failure cannot leave an unassigned child.
    check(unsafe {
        CreateProcessW(
            exe.as_ptr(),
            command.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            1,
            CREATE_NO_WINDOW | EXTENDED_STARTUPINFO_PRESENT,
            std::ptr::null(),
            std::ptr::null(),
            &startup.StartupInfo,
            &mut info,
        )
    })?;
    let process = own(info.hProcess)?;
    let _primary_thread = own(info.hThread)?;
    Ok(NativeProcess {
        process,
        job,
        stdout,
        stderr,
    })
}
