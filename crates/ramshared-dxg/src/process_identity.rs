// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-dxg/src/process_identity.rs
//! Native Linux process incarnation, independent of numeric PID aliases.
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::Arc;

/// Local kernel observation only; not authentication of a remote process.
#[derive(Clone, Debug)]
pub struct LinuxProcessIdentity {
    observed: ProcessObservation,
    lifetime: Arc<OwnedFd>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ProcessObservation {
    pid: u32,
    start_ticks: u64,
    namespace_device: u64,
    namespace_inode: u64,
    boot_id: [u8; 16],
}

const PROC_MAGIC: libc::c_long = 0x9fa0;
const NS_MAGIC: libc::c_long = 0x6e73_6673;
const PROC_STAT_STATE_FIELD: usize = 3;
const PROC_STAT_START_TIME_FIELD: usize = 22;
// The pidfd-capable Linux baseline provides exit_code (field 52). Additional
// future trailing fields are permitted; a shortened record is not complete.
const PROC_STAT_EXIT_CODE_FIELD: usize = 52;

impl LinuxProcessIdentity {
    pub(crate) fn current() -> Option<Self> {
        let observed = ProcessObservation::current()?;
        let identity = Self {
            observed,
            lifetime: Arc::new(open_pidfd(observed.pid)?),
        };
        identity
            .matches_observation(ProcessObservation::current())
            .then_some(identity)
    }

    pub(crate) fn matches_observation(&self, observed: Option<ProcessObservation>) -> bool {
        observed.is_some_and(|current| current == self.observed) && self.is_alive()
    }

    pub(crate) fn is_alive(&self) -> bool {
        pidfd_is_alive(&self.lifetime)
    }
}

// Equality is stable data equality. Every admission must separately validate
// the captured lifetime; a dead capability remains equal to its own clone.
impl PartialEq for LinuxProcessIdentity {
    fn eq(&self, other: &Self) -> bool {
        self.observed == other.observed
    }
}
impl Eq for LinuxProcessIdentity {}

impl ProcessObservation {
    /// Fixed-size stack buffers and native syscalls only, including failure
    /// paths. Keep this observation usable in the native fork regression.
    pub(crate) fn current() -> Option<Self> {
        // SAFETY: getpid has no pointers and observes the calling process.
        let pid = u32::try_from(unsafe { libc::getpid() })
            .ok()
            .filter(|pid| *pid != 0)?;
        let (stat, length) = read_proc::<4096>(c"/proc/self/stat")?;
        let start_ticks = parse_start_ticks(&stat[..length])?;
        let namespace = open_native(c"/proc/self/ns/pid", NS_MAGIC)?;
        let mut metadata = std::mem::MaybeUninit::<libc::stat>::uninit();
        // SAFETY: namespace owns a live descriptor; metadata is correctly
        // sized/aligned writable storage for the native Linux stat ABI.
        if unsafe { libc::fstat(namespace.as_raw_fd(), metadata.as_mut_ptr()) } != 0 {
            return None;
        }
        // SAFETY: successful fstat initialized the complete native structure.
        let metadata = unsafe { metadata.assume_init() };
        if metadata.st_ino == 0 {
            return None;
        }
        let (boot, length) = read_proc::<64>(c"/proc/sys/kernel/random/boot_id")?;
        Some(Self {
            pid,
            start_ticks,
            namespace_device: metadata.st_dev,
            namespace_inode: metadata.st_ino,
            boot_id: parse_boot_id(&boot[..length])?,
        })
    }
}

fn open_pidfd(pid: u32) -> Option<OwnedFd> {
    let pid = libc::pid_t::try_from(pid).ok()?;
    // SAFETY: pidfd_open takes scalar PID/flags arguments and returns a fresh
    // close-on-exec descriptor for that exact kernel process instance.
    let raw = i32::try_from(unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0u32) }).ok()?;
    if raw < 0 {
        return None;
    }
    // SAFETY: the successful syscall transferred one new descriptor to us.
    Some(unsafe { OwnedFd::from_raw_fd(raw) })
}

fn pidfd_is_alive(fd: &OwnedFd) -> bool {
    let mut request = libc::pollfd {
        fd: fd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: request is one live native pollfd. Timeout zero never waits;
    // readiness, errors and interrupted observations all refuse ownership.
    unsafe { libc::poll(&mut request, 1, 0) == 0 && request.revents == 0 }
}

fn open_native(path: &std::ffi::CStr, expected_fs: libc::c_long) -> Option<OwnedFd> {
    // SAFETY: path is a live NUL-terminated native path; no creation flag is
    // used. The fixed namespace path intentionally follows its kernel symlink.
    let raw = unsafe { libc::open(path.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC) };
    if raw < 0 {
        return None;
    }
    // SAFETY: successful open returned a fresh descriptor, transferred once
    // into RAII ownership. Every refusal below closes exactly that descriptor.
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    let mut metadata = std::mem::MaybeUninit::<libc::statfs>::uninit();
    // SAFETY: fd remains owned/live and metadata has the native statfs layout.
    if unsafe { libc::fstatfs(fd.as_raw_fd(), metadata.as_mut_ptr()) } != 0 {
        return None;
    }
    // SAFETY: successful fstatfs initialized the complete structure.
    (unsafe { metadata.assume_init() }.f_type == expected_fs).then_some(fd)
}

fn read_proc<const N: usize>(path: &std::ffi::CStr) -> Option<([u8; N], usize)> {
    let fd = open_native(path, PROC_MAGIC)?;
    let mut bytes = [0; N];
    // SAFETY: fd is owned/live; bytes is writable for exactly N bytes. Refuse
    // partial/truncated records during parsing rather than replaying a read.
    let length = unsafe { libc::read(fd.as_raw_fd(), bytes.as_mut_ptr().cast(), N) };
    let length = usize::try_from(length)
        .ok()
        .filter(|length| *length > 0 && *length < N)?;
    Some((bytes, length))
}

fn decimal(bytes: &[u8]) -> Option<u64> {
    if bytes.is_empty() || !bytes.iter().all(u8::is_ascii_digit) {
        return None;
    }
    bytes.iter().try_fold(0u64, |value, byte| {
        value.checked_mul(10)?.checked_add(u64::from(byte - b'0'))
    })
}

fn parse_start_ticks(bytes: &[u8]) -> Option<u64> {
    if !bytes.ends_with(b"\n") {
        return None;
    }
    let pid_end = bytes.iter().position(|byte| *byte == b' ')?;
    let pid = decimal(&bytes[..pid_end])?;
    if pid == 0 || pid > u64::from(u32::MAX) || bytes.get(pid_end + 1) != Some(&b'(') {
        return None;
    }
    // comm may contain spaces, parentheses, newlines and non-UTF-8 bytes.
    let comm_end = bytes.windows(2).rposition(|pair| pair == b") ")?;
    if comm_end <= pid_end + 1 {
        return None;
    }
    let mut fields = bytes[comm_end + 2..]
        .split(u8::is_ascii_whitespace)
        .filter(|field| !field.is_empty());
    let state = fields.next()?;
    if state.len() != 1 || !state[0].is_ascii_alphabetic() {
        return None;
    }
    let ticks = decimal(fields.nth(PROC_STAT_START_TIME_FIELD - PROC_STAT_STATE_FIELD - 1)?)?;
    fields.nth(PROC_STAT_EXIT_CODE_FIELD - PROC_STAT_START_TIME_FIELD - 1)?;
    Some(ticks)
}

fn parse_boot_id(bytes: &[u8]) -> Option<[u8; 16]> {
    let bytes = bytes.strip_suffix(b"\n").unwrap_or(bytes);
    if bytes.len() != 36 || [8, 13, 18, 23].iter().any(|index| bytes[*index] != b'-') {
        return None;
    }
    let mut result = [0; 16];
    let mut nibbles = 0;
    for (index, byte) in bytes.iter().enumerate() {
        if [8, 13, 18, 23].contains(&index) {
            continue;
        }
        let value = match byte {
            b'0'..=b'9' => byte - b'0',
            b'a'..=b'f' => byte - b'a' + 10,
            _ => return None,
        };
        result[nibbles / 2] = (result[nibbles / 2] << 4) | value;
        nibbles += 1;
    }
    result.iter().any(|byte| *byte != 0).then_some(result)
}

#[cfg(test)]
mod tests;
