// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-winsvc/src/gpu_identity/process/pipe.rs
//! Overlapped pipe capture with stable owned buffers through completion.
use super::super::supervisor::PipeRead;
use super::{check, last_error};
use std::cell::UnsafeCell;
use std::os::windows::io::{AsRawHandle, OwnedHandle};
use windows_sys::Win32::Foundation::{
    ERROR_BROKEN_PIPE, ERROR_IO_INCOMPLETE, ERROR_IO_PENDING, ERROR_PIPE_CONNECTED,
    ERROR_PIPE_NOT_CONNECTED, GENERIC_WRITE,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED,
    OPEN_EXISTING, PIPE_ACCESS_INBOUND, ReadFile,
};
use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS,
    PIPE_TYPE_BYTE, PIPE_WAIT,
};
use windows_sys::Win32::System::Threading::{CreateEventW, ResetEvent};

const BYTES: usize = 4096;
const _: () = assert!(std::mem::size_of::<OVERLAPPED>() == 32);
#[derive(Clone, Copy)]
enum Operation {
    Connect,
    Read,
}
pub(super) struct AsyncPipe {
    handle: OwnedHandle,
    event: OwnedHandle,
    overlapped: Box<UnsafeCell<OVERLAPPED>>,
    buffer: Box<UnsafeCell<[u8; BYTES]>>,
    pending: Option<Operation>,
    closed: bool,
}
// SAFETY: one exclusive owner polls this pipe. OVERLAPPED and buffer allocations
// never move; their only raw references belong to this handle's kernel operation.
// UnsafeCell permits kernel writes while polling; no Rust reference to either
// allocation is formed until GetOverlappedResult proves completion.
unsafe impl Send for AsyncPipe {}

pub(super) fn create() -> Result<(AsyncPipe, OwnedHandle), String> {
    let nonce = ramshared_ipc::random_nonce().map_err(|error| error.to_string())?;
    let name = format!(
        r"\\.\pipe\ramshared-native-{}",
        nonce
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    );
    let name: Vec<u16> = name.encode_utf16().chain([0]).collect();
    // SAFETY: private unique local name, non-inheritable overlapped read end.
    let handle = super::spawn::own(unsafe {
        CreateNamedPipeW(
            name.as_ptr(),
            PIPE_ACCESS_INBOUND | FILE_FLAG_OVERLAPPED | FILE_FLAG_FIRST_PIPE_INSTANCE,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            1,
            BYTES as u32,
            BYTES as u32,
            0,
            std::ptr::null(),
        )
    })?;
    // SAFETY: unnamed non-inheritable manual-reset event for one operation.
    let event =
        super::spawn::own(unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) })?;
    let mut pipe = AsyncPipe {
        handle,
        event,
        overlapped: Box::new(UnsafeCell::new(OVERLAPPED::default())),
        buffer: Box::new(UnsafeCell::new([0; BYTES])),
        pending: None,
        closed: false,
    };
    // SAFETY: connect only to this just-created private local instance. The
    // inheritable write end appears exclusively in the child HANDLE_LIST.
    let writer = super::spawn::own(unsafe {
        CreateFileW(
            name.as_ptr(),
            GENERIC_WRITE,
            0,
            &super::spawn::attributes(),
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            std::ptr::null_mut(),
        )
    })?;
    pipe.prepare()?;
    // SAFETY: live stable OVERLAPPED/event; any pending connect remains owned.
    if unsafe { ConnectNamedPipe(pipe.handle.as_raw_handle(), pipe.overlapped.get()) } == 0 {
        match std::io::Error::last_os_error().raw_os_error() {
            Some(code) if code == ERROR_PIPE_CONNECTED as i32 => {}
            Some(code) if code == ERROR_IO_PENDING as i32 => {
                pipe.pending = Some(Operation::Connect)
            }
            _ => return Err(last_error()),
        }
    }
    Ok((pipe, writer))
}
impl AsyncPipe {
    fn prepare(&mut self) -> Result<(), String> {
        // SAFETY: no operation is pending when its state/event is reset.
        check(unsafe { ResetEvent(self.event.as_raw_handle()) })?;
        // SAFETY: no pending request can access this stable UnsafeCell here.
        unsafe {
            *self.overlapped.get() = OVERLAPPED {
                hEvent: self.event.as_raw_handle(),
                ..OVERLAPPED::default()
            };
        }
        Ok(())
    }
    pub(super) fn read(&mut self, output: &mut [u8]) -> Result<PipeRead, String> {
        if self.closed {
            return Ok(PipeRead::Closed);
        }
        if let Some(operation) = self.pending {
            let mut bytes = 0;
            // SAFETY: exact outstanding handle/OVERLAPPED; FALSE never waits.
            let ok = unsafe {
                GetOverlappedResult(
                    self.handle.as_raw_handle(),
                    self.overlapped.get(),
                    &mut bytes,
                    0,
                )
            };
            if ok == 0
                && std::io::Error::last_os_error().raw_os_error()
                    == Some(ERROR_IO_INCOMPLETE as i32)
            {
                return Ok(PipeRead::Pending);
            }
            self.pending = None;
            if ok == 0 {
                return self.read_error();
            }
            if matches!(operation, Operation::Read) {
                return self.copy(bytes, output);
            }
        }
        if output.is_empty() || output.len() > BYTES {
            return Err("async pipe buffer violates its read bound".into());
        }
        self.prepare()?;
        // SAFETY: stable private initialized buffer and OVERLAPPED/event. On
        // IO_PENDING both allocations stay owned and untouched until completion.
        // Async byte counts come only from GetOverlappedResult, never a stack
        // output passed to ReadFile.
        let ok = unsafe {
            ReadFile(
                self.handle.as_raw_handle(),
                self.buffer.get().cast::<u8>(),
                output.len() as u32,
                std::ptr::null_mut(),
                self.overlapped.get(),
            )
        };
        if ok != 0
            || std::io::Error::last_os_error().raw_os_error() == Some(ERROR_IO_PENDING as i32)
        {
            self.pending = Some(Operation::Read);
            return Ok(PipeRead::Pending);
        }
        self.read_error()
    }
    fn copy(&mut self, bytes: u32, output: &mut [u8]) -> Result<PipeRead, String> {
        let count = bytes as usize;
        if count > output.len() || count > BYTES {
            return Err("async pipe completion exceeds buffer".into());
        }
        if count == 0 {
            self.closed = true;
            return Ok(PipeRead::Closed);
        }
        // SAFETY: completion was observed before copying; the kernel no longer
        // writes this private buffer and the checked range is within its bound.
        let completed = unsafe { &*self.buffer.get() };
        output[..count].copy_from_slice(&completed[..count]);
        Ok(PipeRead::Bytes(count))
    }
    fn read_error(&mut self) -> Result<PipeRead, String> {
        let error = std::io::Error::last_os_error();
        self.closed = true;
        if [
            Some(ERROR_BROKEN_PIPE as i32),
            Some(ERROR_PIPE_NOT_CONNECTED as i32),
        ]
        .contains(&error.raw_os_error())
        {
            Ok(PipeRead::Closed)
        } else {
            Err(error.to_string())
        }
    }
}
impl Drop for AsyncPipe {
    fn drop(&mut self) {
        if self.pending.is_some() {
            // Exceptional scoped/unwind cleanup must not free memory still used
            // by kernel IO. Production custody normally reaches settled reads
            // first. Cancellation is a request; wait for its actual completion.
            // SAFETY: exact owned pending operation, buffers/handle/event remain
            // live through cancellation and the completion observation below.
            unsafe {
                CancelIoEx(self.handle.as_raw_handle(), self.overlapped.get());
                let mut bytes = 0;
                GetOverlappedResult(
                    self.handle.as_raw_handle(),
                    self.overlapped.get(),
                    &mut bytes,
                    1,
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    #[test]
    fn async_pipe_keeps_buffer_through_pending_cancel_completion() {
        let (mut pipe, writer) = create().unwrap();
        let mut output = [0; BYTES];
        assert!(matches!(pipe.read(&mut output).unwrap(), PipeRead::Pending));
        assert!(pipe.pending.is_some());
        drop(pipe);
        // Writer was open during cancellation; EOF could not explain completion.
        drop(writer);
    }
}
