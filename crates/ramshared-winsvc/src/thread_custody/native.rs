// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-winsvc/src/thread_custody/native.rs
//! Observe the actual kernel thread, including thread-local cleanup, before join.
use std::io;
use std::os::windows::io::AsRawHandle;
use std::thread::JoinHandle;
use windows_sys::Win32::Foundation::{WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows_sys::Win32::System::Threading::WaitForSingleObject;

pub(super) fn has_exited<T>(worker: &JoinHandle<T>) -> io::Result<bool> {
    // SAFETY: borrowed live thread handle remains owned by this JoinHandle;
    // zero wait does not block, close, or consume the native thread handle.
    match unsafe { WaitForSingleObject(worker.as_raw_handle(), 0) } {
        WAIT_OBJECT_0 => Ok(true),
        WAIT_TIMEOUT => Ok(false),
        WAIT_FAILED => Err(io::Error::last_os_error()),
        _ => Err(io::Error::other("unexpected native thread observation")),
    }
}
