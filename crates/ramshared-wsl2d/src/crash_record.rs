// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/crash_record.rs
//! Durable crash record for the swap peer.
//!
//! The peer is the only thing standing between the kernel and a guest-wide
//! SIGBUS storm: when it dies, every process holding a page on the tier
//! faults. The peer's stderr used to live in tmpfs, so the next boot had no
//! evidence of *why* the peer died — only the kernel's "fatal signal 7" line
//! and the tier's I/O errors. This recorder writes one small, bounded record
//! to a durable file *before* the process dies, from the signal handler
//! itself.
//!
//! The handler is async-signal-safe by construction: it formats into a stack
//! buffer with no allocation, performs one `write(2)`, restores the default
//! disposition, and re-raises the signal so the exit status stays the one
//! every other tool expects (death by signal, not a silent exit code).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};

/// The open crash-log descriptor, or `-1` when the recorder is not installed.
static CRASH_FD: AtomicI32 = AtomicI32::new(-1);

/// Set by the handler when a fatal signal arrived.
static CRASH_REQUESTED: AtomicBool = AtomicBool::new(false);

/// Set when the release watchdog is armed. The handler only waits for a
/// release that can actually happen.
static RELEASE_ARMED: AtomicBool = AtomicBool::new(false);

/// Set by the release watchdog when the managed tier is no longer armed.
///
/// The tier is a **swap** device: every process that faults a page onto it
/// while its peer is gone takes a SIGBUS instead of a recoverable error. A
/// peer that dies must therefore not leave the tier armed, and the only
/// moment it can do that is between the fatal signal and its own death.
static RELEASE_FINISHED: AtomicBool = AtomicBool::new(false);

/// How long the handler waits for the release before it lets the process die
/// anyway. The wait is what turns a guest-wide SIGBUS storm into a clean
/// release; it is bounded because a host that cannot release the tier must
/// not keep the process alive forever.
const RELEASE_WAIT_LIMIT_MS: u64 = 10_000;

/// Whether a fatal signal has been observed.
pub(super) fn crash_requested() -> bool {
    CRASH_REQUESTED.load(Ordering::Acquire)
}

/// Whether the managed tier has been released (or the release was given up).
pub(super) fn release_finished() -> bool {
    RELEASE_FINISHED.load(Ordering::Acquire)
}

/// Marks the managed tier as released.
pub(super) fn mark_release_finished() {
    RELEASE_FINISHED.store(true, Ordering::Release);
}

/// Longest record this recorder can emit. The handler writes at most this
/// many bytes and never grows a buffer.
pub(super) const RECORD_MAX_BYTES: usize = 256;

/// The longest thread name `prctl(PR_GET_NAME)` can return.
const THREAD_NAME_MAX: usize = 16;

/// Signals that are recorded. `SIGBUS` is the one the peer actually died of
/// on 2026-10-20; the others are the neighbouring fatal faults that would
/// leave the same guest-wide hole behind.
const RECORDED_SIGNALS: [i32; 4] = [libc::SIGBUS, libc::SIGSEGV, libc::SIGILL, libc::SIGFPE];

/// Default durable location of the crash record.
pub(super) const DEFAULT_CRASH_LOG: &str = "/var/log/ramshared/ramsharedd-crash.log";

/// Fallback when the durable directory is not writable (read-only root, a
/// container without `/var/log`, a unit sandbox).
pub(super) const FALLBACK_CRASH_LOG: &str = "/run/ramshared/ramsharedd-crash.log";

/// Bound on the crash log before the next open rotates it to `.1`.
///
/// A record is at most 256 bytes, so this holds several hundred faults. The
/// freeze that motivated the recorder produced nine records in one boot, but
/// a repeating storm must not grow an unbounded durable file: the evidence
/// that matters is the first fault, not the ten-thousandth repetition.
const CRASH_LOG_ROTATE_BYTES: u64 = 64 * 1024;

/// Rotates an oversized crash log to `<path>.1` before it is reopened.
///
/// The record has to survive the death that follows the fault, so rotation is
/// a bounded rename, not a truncate: the previous generation stays readable
/// for the postmortem. A path that is a symlink or not a regular file is
/// refused rather than followed.
fn rotate_before_open(path: &Path) -> Result<(), String> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(format!("inspect crash log {}: {error}", path.display()));
        }
    };
    if !metadata.file_type().is_file() {
        return Err(format!(
            "crash log {} is not a regular file",
            path.display()
        ));
    }
    if metadata.len() <= CRASH_LOG_ROTATE_BYTES {
        return Ok(());
    }
    let mut rotated = path.as_os_str().to_os_string();
    rotated.push(".1");
    let rotated = PathBuf::from(rotated);
    std::fs::rename(path, &rotated).map_err(|error| {
        format!(
            "rotate crash log {} to {}: {error}",
            path.display(),
            rotated.display()
        )
    })
}

/// Installs the recorder.
///
/// The daemon calls this before it touches the GPU, the tier or the NBD
/// socket, so a fault in any of those still leaves a record. A failure to
/// install is not fatal: the daemon must still serve, and the kernel's own
/// "fatal signal" line remains the backstop.
pub(super) fn install(path: &Path) -> Result<(), String> {
    install_with(path, |signal| {
        // SAFETY: the handler is async-signal-safe (see the module comment),
        // and `sigaction` is called with a valid, zeroed action and a valid
        // handler pointer.
        let result = unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = crash_handler as *const () as usize;
            action.sa_flags = libc::SA_SIGINFO | libc::SA_RESTART;
            libc::sigemptyset(&mut action.sa_mask);
            libc::sigaction(signal, &action, std::ptr::null_mut())
        };
        if result != 0 {
            return Err(format!(
                "install crash handler for signal {signal}: {}",
                std::io::Error::last_os_error()
            ));
        }
        Ok(())
    })
}

fn install_with(
    path: &Path,
    mut register: impl FnMut(i32) -> Result<(), String>,
) -> Result<(), String> {
    rotate_before_open(path)?;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|error| format!("open crash log {}: {error}", path.display()))?;
    use std::os::unix::io::IntoRawFd;
    for signal in RECORDED_SIGNALS {
        register(signal)?;
    }
    // Failed registration drops the unpublished File. Only a fully installed
    // recorder transfers the descriptor to its process-lifetime signal handler.
    let owned_fd = file.into_raw_fd();
    CRASH_FD.store(owned_fd, Ordering::Release);
    Ok(())
}

/// Installs the recorder at the durable path, falling back to the runtime
/// path when the durable one cannot be opened.
pub(super) fn install_default() -> Option<(std::path::PathBuf, Result<(), String>)> {
    let durable = Path::new(DEFAULT_CRASH_LOG);
    match install(durable) {
        Ok(()) => Some((durable.to_path_buf(), Ok(()))),
        Err(durable_error) => {
            let fallback = Path::new(FALLBACK_CRASH_LOG);
            match install(fallback) {
                Ok(()) => Some((fallback.to_path_buf(), Ok(()))),
                Err(fallback_error) => Some((
                    durable.to_path_buf(),
                    Err(format!(
                        "{durable_error}; fallback failed too: {fallback_error}"
                    )),
                )),
            }
        }
    }
}

/// The signal handler.
///
/// It writes one record and re-raises. Nothing else happens here: no
/// allocation, no locking, no formatting library, no logging framework.
extern "C" fn crash_handler(signal: i32, info: *mut libc::siginfo_t, _ctx: *mut libc::c_void) {
    handle_fatal_signal(signal, info);
    // Restore the default disposition and re-raise so the process dies of
    // the original signal: the exit status must keep saying SIGBUS.
    re_raise(signal);
}

/// The whole of the handler except the re-raise.
///
/// It is one function so the policy (record the fault, wait for the tier
/// release) can be exercised without a process that dies of the signal it
/// records.
fn handle_fatal_signal(signal: i32, info: *mut libc::siginfo_t) {
    let mut code: i32 = 0;
    let mut address: u64 = 0;
    if !info.is_null() {
        // SAFETY: the kernel passes a valid `siginfo_t` for SA_SIGINFO
        // handlers, and the two fields read here are plain integers.
        let signal_info = unsafe { *info };
        code = signal_info.si_code;
        // SAFETY: reading the faulting address out of the `siginfo_t`
        // the kernel handed us.
        address = unsafe { signal_info.si_addr() } as u64;
    }
    let mut name = [0u8; THREAD_NAME_MAX];
    // SAFETY: `prctl` with PR_GET_NAME writes at most 16 bytes into the
    // buffer we own.
    unsafe {
        libc::prctl(libc::PR_GET_NAME, name.as_mut_ptr(), 0, 0, 0);
    }
    let tid = unsafe { libc::syscall(libc::SYS_gettid) } as i32;
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0);
    record_fault(signal, code, address, tid, now_ms, &name);
    // The tier must not stay armed behind a dead peer. The release runs on
    // the watchdog thread; this thread waits for it, because the moment the
    // process dies the tier would be left armed with a dead peer — the exact
    // state that turned one peer death into a guest-wide SIGBUS storm
    //. The wait is bounded: a host that cannot release the tier
    // must not keep the process alive forever.
    await_release();
}

/// Writes the durable crash record. One `write(2)`, no allocation.
fn record_fault(signal: i32, code: i32, address: u64, tid: i32, now_ms: u64, thread_name: &[u8]) {
    let fd = CRASH_FD.load(Ordering::Acquire);
    if fd < 0 {
        return;
    }
    let (record, length) = format_record(signal, code, address, tid, now_ms, thread_name);
    // SAFETY: a single `write(2)` to an owned append-mode descriptor is
    // async-signal-safe.
    unsafe {
        libc::write(fd, record.as_ptr() as *const libc::c_void, length);
    }
}

/// Waits, bounded, for the release watchdog to finish the tier release.
fn await_release() {
    CRASH_REQUESTED.store(true, Ordering::Release);
    if !RELEASE_ARMED.load(Ordering::Acquire) {
        return;
    }
    let deadline =
        std::time::Instant::now() + std::time::Duration::from_millis(RELEASE_WAIT_LIMIT_MS);
    while !release_finished() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

/// Restores the default disposition and re-raises the signal.
fn re_raise(signal: i32) {
    // SAFETY: `signal` with SIG_DFL and a subsequent `raise` are both
    // async-signal-safe.
    unsafe {
        libc::signal(signal, libc::SIG_DFL);
        libc::raise(signal);
    }
}

/// Renders one crash record.
///
/// The format is deliberately fixed-width and hex: it is written from a
/// signal handler, where a formatting library would allocate. The fields are
/// what an operator needs to line the record up with the kernel's own
/// "fatal signal" line and with the tier's I/O errors.
pub(super) fn format_record(
    signal: i32,
    code: i32,
    address: u64,
    tid: i32,
    now_ms: u64,
    thread_name: &[u8],
) -> ([u8; RECORD_MAX_BYTES], usize) {
    let mut buffer = [0u8; RECORD_MAX_BYTES];
    let mut cursor = 0usize;
    push_bytes(&mut buffer, &mut cursor, b"RSCR1 signal=0x");
    push_hex(&mut buffer, &mut cursor, signal as u32 as u64, false);
    push_bytes(&mut buffer, &mut cursor, b" code=0x");
    push_hex(&mut buffer, &mut cursor, code as u32 as u64, false);
    push_bytes(&mut buffer, &mut cursor, b" addr=0x");
    push_hex(&mut buffer, &mut cursor, address, true);
    push_bytes(&mut buffer, &mut cursor, b" tid=0x");
    push_hex(&mut buffer, &mut cursor, tid as u32 as u64, false);
    push_bytes(&mut buffer, &mut cursor, b" ts_ms=0x");
    push_hex(&mut buffer, &mut cursor, now_ms, true);
    push_bytes(&mut buffer, &mut cursor, b" name=");
    for byte in thread_name
        .iter()
        .copied()
        .take(THREAD_NAME_MAX)
        .take_while(|byte| *byte != 0)
        .filter(|byte| byte.is_ascii_graphic() || *byte == b' ')
    {
        push_bytes(&mut buffer, &mut cursor, &[byte]);
    }
    push_bytes(&mut buffer, &mut cursor, b"\n");
    (buffer, cursor)
}

/// Appends bytes, stopping at the buffer's end.
fn push_bytes(buffer: &mut [u8; RECORD_MAX_BYTES], cursor: &mut usize, bytes: &[u8]) {
    for byte in bytes {
        if *cursor < buffer.len() {
            buffer[*cursor] = *byte;
            *cursor += 1;
        }
    }
}

/// Appends `value` as lowercase hex. `padded` writes the full 16 digits, so
/// an address keeps its width and two records line up column by column.
fn push_hex(buffer: &mut [u8; RECORD_MAX_BYTES], cursor: &mut usize, value: u64, padded: bool) {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut digits = [b'0'; 16];
    let mut index = 16;
    let mut remaining = value;
    loop {
        index -= 1;
        digits[index] = DIGITS[(remaining & 0xF) as usize];
        remaining >>= 4;
        if remaining == 0 {
            break;
        }
    }
    let start = if padded { 0 } else { index };
    for digit in digits.iter().skip(start) {
        if *cursor < buffer.len() {
            buffer[*cursor] = *digit;
            *cursor += 1;
        }
    }
}

/// Arms the managed-tier release watchdog.
///
/// The peer owns a **swap** device. When the peer dies, every process that
/// faults a page onto that device takes a SIGBUS instead of a recoverable
/// error, and the guest can die with it.
///
/// The watchdog therefore waits for the fatal signal and releases the tier
/// before the process is allowed to die. The release is bounded: a host that
/// cannot release the tier must not keep the process alive forever.
pub(super) fn arm_release_watchdog<F>(release: F)
where
    F: Fn() -> bool + Send + 'static,
{
    // The handler only waits for a release that can actually happen, so the
    // armed flag is set here, before the thread exists.
    RELEASE_ARMED.store(true, Ordering::Release);
    eprintln!("[ramsharedd] the managed tier release watchdog is armed");
    std::thread::spawn(move || {
        loop {
            if crash_requested() {
                eprintln!(
                    "[ramsharedd] a fatal signal arrived; attempting to release the managed tier \
                     before the peer dies"
                );
                let released = release();
                mark_release_finished();
                if !released {
                    eprintln!(
                        "[ramsharedd] the managed tier could not be released before the peer died; \
                     the guest may fault pages onto a dead tier"
                    );
                }
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    });
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    /// The recorder's state is process-wide, so the tests that touch it are
    /// serialized: two of them in parallel would read each other's flags.
    static STATE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn failed_signal_registration_closes_only_the_unpublished_log_descriptor() {
        use std::os::unix::fs::MetadataExt;
        let _state = STATE_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let path = temp_path("registration-failure");
        std::fs::write(&path, b"previous record\n").unwrap();
        let original = std::fs::metadata(&path).unwrap();
        let published = CRASH_FD.load(Ordering::Acquire);
        let mut calls = 0;
        let mut log_fd = None;
        let result = install_with(&path, |_| {
            calls += 1;
            if calls == 2 {
                log_fd = std::fs::read_dir("/proc/self/fd")
                    .unwrap()
                    .filter_map(Result::ok)
                    .map(|entry| entry.path())
                    .find(|fd| std::fs::read_link(fd).ok().as_ref() == Some(&path));
                return Err("injected registration failure".into());
            }
            Ok(())
        });
        let log_fd = log_fd.expect("the unpublished descriptor must exist during registration");
        let leaked = std::fs::metadata(log_fd)
            .ok()
            .is_some_and(|meta| meta.dev() == original.dev() && meta.ino() == original.ino());
        let bytes = std::fs::read(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert!(result.is_err());
        assert_eq!(calls, 2);
        assert_eq!(CRASH_FD.load(Ordering::Acquire), published);
        assert_eq!(bytes, b"previous record\n");
        assert!(
            !leaked,
            "failed registration must retain RAII ownership until close"
        );
    }

    use super::*;
    use std::os::unix::process::ExitStatusExt;

    thread_local! {
        static HEAP_CALLS: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
    }

    struct ObservedAllocator;

    fn observe_heap_call() {
        let _ = HEAP_CALLS.try_with(|calls| {
            if let Some(count) = calls.get() {
                calls.set(Some(count.saturating_add(1)));
            }
        });
    }

    // This binary's test allocator observes only the current opt-in thread.
    // Const TLS access and integer arithmetic do not recurse into allocation.
    #[global_allocator]
    static TEST_ALLOCATOR: ObservedAllocator = ObservedAllocator;

    // SAFETY: every allocation operation is forwarded to the same System
    // allocator with unchanged layout/pointer arguments and returned pointer.
    unsafe impl std::alloc::GlobalAlloc for ObservedAllocator {
        unsafe fn alloc(&self, layout: std::alloc::Layout) -> *mut u8 {
            observe_heap_call();
            // SAFETY: the caller supplies the GlobalAlloc layout contract.
            unsafe { std::alloc::System.alloc(layout) }
        }

        unsafe fn alloc_zeroed(&self, layout: std::alloc::Layout) -> *mut u8 {
            observe_heap_call();
            // SAFETY: the caller supplies the GlobalAlloc layout contract.
            unsafe { std::alloc::System.alloc_zeroed(layout) }
        }

        unsafe fn realloc(&self, ptr: *mut u8, layout: std::alloc::Layout, size: usize) -> *mut u8 {
            observe_heap_call();
            // SAFETY: the caller supplies a live System pointer and valid sizes.
            unsafe { std::alloc::System.realloc(ptr, layout, size) }
        }

        unsafe fn dealloc(&self, ptr: *mut u8, layout: std::alloc::Layout) {
            // SAFETY: the caller supplies the original allocation and layout.
            unsafe { std::alloc::System.dealloc(ptr, layout) }
        }
    }

    #[test]
    fn fault_formatting_never_enters_the_allocator() {
        for name in [
            &b"ramsharedd\0"[..],
            &b"a\n\xff b\0ignored"[..],
            &[b'x'; 512][..],
            &[],
        ] {
            HEAP_CALLS.with(|calls| calls.set(Some(0)));
            let (record, length) = format_record(libc::SIGBUS, 2, 0, 1, 0, name);
            let allocations = HEAP_CALLS.with(|calls| calls.replace(None));
            assert_eq!(
                allocations,
                Some(0),
                "fatal record formatting must not acquire allocator locks"
            );
            let text = std::str::from_utf8(&record[..length]).unwrap();
            assert!(text.starts_with("RSCR1 signal=0x7 "));
            assert!(
                text.ends_with('\n'),
                "every bounded record must retain its delimiter"
            );
            assert!(!text.contains("ignored"));
            assert!(!text[..text.len().saturating_sub(1)].contains('\n'));
        }
    }

    #[test]
    fn allocation_probe_observes_a_real_allocation() {
        HEAP_CALLS.with(|calls| calls.set(Some(0)));
        let allocation = std::hint::black_box(Box::new(42u64));
        let calls = HEAP_CALLS.with(|calls| calls.replace(None));
        assert_eq!(*allocation, 42);
        assert!(
            calls.is_some_and(|count| count > 0),
            "a zero-only counter cannot prove signal safety"
        );
    }

    fn temp_path(name: &str) -> std::path::PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!("ramshared-crash-{name}-{}", std::process::id()));
        path
    }

    #[test]
    fn the_record_names_the_signal_the_code_and_the_address() {
        let (record, length) = format_record(
            libc::SIGBUS,
            2,
            0x0000_7fff_1234_5678,
            4242,
            0x0000_0001_9a2b_3c4d,
            b"ramsharedd\0",
        );
        let text = String::from_utf8_lossy(&record[..length]);
        assert!(text.starts_with("RSCR1 signal=0x7 "), "{text}");
        assert!(text.contains(" code=0x2 "), "{text}");
        assert!(text.contains(" addr=0x00007fff12345678"), "{text}");
        assert!(text.contains(" tid=0x1092"), "{text}");
        assert!(text.contains(" ts_ms=0x000000019a2b3c4d"), "{text}");
        assert!(text.contains(" name=ramsharedd"), "{text}");
        assert!(text.ends_with('\n'), "{text}");
    }

    #[test]
    fn the_record_truncates_a_long_thread_name_instead_of_overflowing() {
        let long = [b'x'; 64];
        let (record, length) = format_record(libc::SIGSEGV, 1, 0, 1, 0, &long);
        let text = String::from_utf8_lossy(&record[..length]);
        assert!(text.contains(" name="), "{text}");
        assert!(record.len() <= RECORD_MAX_BYTES);
        // The name is bounded by the kernel's 16-byte limit, and the record
        // never grows past its buffer.
        assert!(text.len() < RECORD_MAX_BYTES, "{text}");
    }

    #[test]
    fn the_release_watchdog_releases_the_tier_when_a_fatal_signal_arrives() {
        let _state = STATE_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // The tier must not stay armed behind a dead peer: every process that
        // faults a page onto it takes a SIGBUS. The watchdog runs the release
        // and reports whether it succeeded.
        let released = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = std::sync::Arc::clone(&released);
        arm_release_watchdog(move || {
            flag.store(true, std::sync::atomic::Ordering::SeqCst);
            true
        });
        // The flag is what the signal handler sets; the test drives the same
        // path the handler does.
        CRASH_REQUESTED.store(true, std::sync::atomic::Ordering::Release);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !release_finished() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(release_finished(), "the watchdog must finish the release");
        assert!(
            released.load(std::sync::atomic::Ordering::SeqCst),
            "the release must have run"
        );
        CRASH_REQUESTED.store(false, std::sync::atomic::Ordering::Release);
        RELEASE_FINISHED.store(false, std::sync::atomic::Ordering::Release);
        RELEASE_ARMED.store(false, std::sync::atomic::Ordering::Release);
    }

    #[test]
    fn the_crash_log_rotates_before_the_next_open() {
        let _state = STATE_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // A storm must not grow the durable file unbounded, and the previous
        // generation must survive for the postmortem.
        let path = temp_path("rotate");
        let mut rotated = path.as_os_str().to_os_string();
        rotated.push(".1");
        let rotated = PathBuf::from(rotated);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&rotated);
        std::fs::write(&path, vec![b'x'; CRASH_LOG_ROTATE_BYTES as usize + 1])
            .expect("seed an oversized crash log");
        install(&path).expect("install over an oversized crash log");
        let preserved = std::fs::read(&rotated).expect("the previous generation is preserved");
        assert_eq!(preserved.len(), CRASH_LOG_ROTATE_BYTES as usize + 1);
        let reopened = std::fs::metadata(&path).expect("the new log is created");
        assert_eq!(reopened.len(), 0);

        // Exactly at the bound is still openable without a rotation.
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&rotated);
        std::fs::write(&path, vec![b'y'; CRASH_LOG_ROTATE_BYTES as usize])
            .expect("seed a bounded crash log");
        install(&path).expect("install over a bounded crash log");
        assert!(!rotated.exists(), "a log at the bound must not rotate");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn the_recorder_writes_the_fault_to_its_durable_file() {
        let _state = STATE_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // The record is the peer's own account of the fault, and it has to
        // survive the death that follows it.
        let path = temp_path("record");
        let _ = std::fs::remove_file(&path);
        install(&path).expect("install the recorder");
        record_fault(
            libc::SIGBUS,
            2,
            0x0000_7fff_1234_5678,
            4242,
            0x0000_0001_9a2b_3c4d,
            b"ramsharedd\0",
        );
        let written = std::fs::read_to_string(&path).expect("the record is durable");
        assert!(written.contains("signal=0x7"), "{written}");
        assert!(written.contains("code=0x2"), "{written}");
        assert!(written.contains("addr=0x00007fff12345678"), "{written}");
        assert!(written.contains("name=ramsharedd"), "{written}");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn the_handler_records_the_fault_and_waits_for_the_release() {
        let _state = STATE_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let path = temp_path("handler");
        let _ = std::fs::remove_file(&path);
        install(&path).expect("install the recorder");
        // The faulting address and the code are read from the `siginfo_t` the
        // kernel hands over; a zeroed one is what a delivered signal carries
        // for a kill.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        info.si_code = 2;
        RELEASE_ARMED.store(true, std::sync::atomic::Ordering::Release);
        handle_fatal_signal(libc::SIGBUS, &mut info);
        let written = std::fs::read_to_string(&path).expect("the record is durable");
        assert!(written.contains("signal=0x7"), "{written}");
        assert!(written.contains("code=0x2"), "{written}");
        RELEASE_ARMED.store(false, std::sync::atomic::Ordering::Release);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn the_release_wait_returns_when_the_watchdog_finishes() {
        let _state = STATE_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // The wait is the policy between the fatal signal and the death: it
        // must not block forever, and it must not return before the release.
        let finished = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = std::sync::Arc::clone(&finished);
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(120));
            flag.store(true, std::sync::atomic::Ordering::SeqCst);
            mark_release_finished();
        });
        RELEASE_ARMED.store(true, std::sync::atomic::Ordering::Release);
        let started = std::time::Instant::now();
        await_release();
        assert!(
            started.elapsed() >= std::time::Duration::from_millis(100),
            "the wait must actually wait for the release"
        );
        RELEASE_FINISHED.store(false, std::sync::atomic::Ordering::Release);
    }

    #[test]
    fn a_release_that_cannot_run_is_reported_as_a_failure() {
        // A tier that cannot be released must be reported, not hidden: the
        // guest may fault pages onto a dead tier.
        assert!(
            !crate::tier_release::release_managed_tier_on_fatal(
                "/dev/ramshared-no-such-device",
                std::time::Duration::from_millis(500)
            ),
            "a missing device cannot be released"
        );
    }

    /// The child half of the end-to-end crash test. It runs only when the
    /// parent sets the environment variable, and it is the child that dies.
    #[test]
    fn a_child_that_faults_leaves_a_record_and_dies_of_the_signal() {
        let Some(path) = std::env::var_os("RAMSHARED_CRASH_CHILD_LOG") else {
            return;
        };
        install(Path::new(&path)).expect("install crash recorder");
        // SAFETY: raising SIGBUS in a process whose handler records it is the
        // behaviour under test.
        unsafe {
            libc::raise(libc::SIGBUS);
        }
        // The handler re-raises with the default disposition, so the process
        // must not come back.
        std::process::exit(0);
    }

    #[test]
    fn a_faulted_child_leaves_a_durable_record_and_dies_of_the_signal() {
        if std::env::var_os("RAMSHARED_CRASH_CHILD_LOG").is_some() {
            return; // the child half
        }
        let path = temp_path("e2e");
        let _ = std::fs::remove_file(&path);
        let mut child = std::process::Command::new(std::env::current_exe().expect("test binary"))
            .args([
                "--exact",
                "crash_record::tests::a_child_that_faults_leaves_a_record_and_dies_of_the_signal",
                "--nocapture",
            ])
            .env("RAMSHARED_CRASH_CHILD_LOG", &path)
            .spawn()
            .expect("spawn child");
        let status = child.wait().expect("wait for child");
        // The child must die of the signal, not exit normally: the recorder
        // restores the default disposition and re-raises.
        let killed = status.signal() == Some(libc::SIGBUS);
        assert!(killed, "child exit status: {status:?}");
        let record = std::fs::read_to_string(&path).expect("crash record");
        assert!(record.contains("RSCR1 signal=0x7"), "{record}");
        assert!(record.contains("name="), "{record}");
        let _ = std::fs::remove_file(&path);
    }
}
