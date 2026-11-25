// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-agent/src/swap.rs
//! Swap execution over NBD: connects `nbd-client`, formats with `mkswap` (DT-16), and activates
//! with `swapon` (priority DT-7); the reverse path deactivates and disconnects. The argv construction
//! is pure/testable; functions that spawn processes are thin wrappers (`Command`) and validated
//! live in qemu / isolated VM drills (not on WSL2 — session rule).
//!
//! DT-14: `nbd-client` ALWAYS with `-timeout 30` and NEVER `-persist` (no auto-reconnect; the
//! broker re-subscribes). DT-16: `mkswap` is mandatory at each attach (VRAM returns zeroed/dirty).
//!
//! Every effect runs under a deadline through `ramshared_exec` (AUD-11): the helper is the leader
//! of its own process group and a timeout contains that group, so a stuck `nbd-client`/`swapon`
//! cannot hang the agent's execution thread. A timeout is not a refusal — the kernel may already
//! have taken the effect — so it is reported as such and the device is retained.

use std::process::Command;
use std::time::Duration;

use ramshared_broker::protocol::NbdEndpoint;
use ramshared_exec::{DEFAULT_OUTPUT_LIMIT, ProcessSpawnError, run_capture_command};

/// Deadline for a forward swap effect (`nbd-client` connect, `mkswap`, `swapon`).
/// Matches the DT-14 NBD I/O bound: the slowest legitimate leg of an attach is
/// the 30 s `nbd-client` I/O timeout, and a helper still stuck past that is not
/// making progress (AUD-11).
const EFFECT_TIMEOUT: Duration = Duration::from_secs(30);

/// Deadline for a teardown that must page an active swap device back in.
/// Matches the cascade's established `SWAPOFF_TIMEOUT`: a healthy
/// `swapoff` of a busy device legitimately runs long, so the bound is generous
/// — but it is still a bound, because a stuck child is what it must contain.
const SWAPOFF_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug, PartialEq, Eq)]
pub enum ResizeError {
    TooSmall(u64),
    ExceedsDiskSpace { size: u64, max: u64 },
}

impl std::fmt::Display for ResizeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ResizeError::TooSmall(size) => {
                write!(f, "swap size {} bytes is below 64 MiB minimum", size)
            }
            ResizeError::ExceedsDiskSpace { size, max } => {
                write!(
                    f,
                    "swap size {} bytes exceeds physical disk space {} bytes",
                    size, max
                )
            }
        }
    }
}

impl std::error::Error for ResizeError {}

pub fn validate_swap_resize(
    size_bytes: u64,
    disk_space_bytes: u64,
) -> std::result::Result<(), ResizeError> {
    const MIN_SWAP_BYTES: u64 = 64 * 1024 * 1024;
    if size_bytes < MIN_SWAP_BYTES {
        return Err(ResizeError::TooSmall(size_bytes));
    }
    if size_bytes > disk_space_bytes {
        return Err(ResizeError::ExceedsDiskSpace {
            size: size_bytes,
            max: disk_space_bytes,
        });
    }
    Ok(())
}

/// Assembles `nbd-client` argv to attach `export` to `dev` (DT-14: `-timeout 30`, no
/// `-persist`). Unix uses `-unix <path>`; TCP uses positional `<host> <port>`.
pub fn nbd_args(endpoint: &NbdEndpoint, export: &str, dev: &str) -> Vec<String> {
    match endpoint {
        NbdEndpoint::Unix { path } => vec![
            "-N".into(),
            export.into(),
            "-unix".into(),
            path.clone(),
            dev.into(),
            "-timeout".into(),
            "30".into(),
        ],
        NbdEndpoint::Tcp { host, port } => vec![
            "-N".into(),
            export.into(),
            host.clone(),
            port.to_string(),
            dev.into(),
            "-timeout".into(),
            "30".into(),
        ],
    }
}

/// Assembles `swapon` argv (DT-7: only emits `-p <prio>` when priority is defined).
pub fn swapon_args(dev: &str, prio: Option<i32>) -> Vec<String> {
    let mut a = Vec::new();
    if let Some(p) = prio {
        a.push("-p".to_string());
        a.push(p.to_string());
    }
    a.push(dev.to_string());
    a
}

/// Runs one swap effect under a deadline, as the leader of its own process
/// group (AUD-11).
///
/// The child is contained at `timeout` by killing exactly its process group and
/// reaping it, so an inherited stdout pipe or a grandchild cannot keep the call
/// open. The in-loop watchdog cannot interrupt a blocked `Command::status()`;
/// the bound has to live here.
fn run(cmd: &str, args: &[String], timeout: Duration) -> std::result::Result<(), SwapError> {
    let mut command = Command::new(cmd);
    command.args(args);
    let label = if args.is_empty() {
        cmd.to_string()
    } else {
        format!("{cmd} {}", args.join(" "))
    };
    match run_capture_command(&mut command, &label, timeout, DEFAULT_OUTPUT_LIMIT, |_| {}) {
        Ok(_) => Ok(()),
        Err(ProcessSpawnError::ExecutionTimeout { timeout, .. }) => {
            Err(SwapError::Timeout(format!(
                "timed out after {} ms; process group contained",
                timeout.as_millis()
            )))
        }
        Err(ProcessSpawnError::NonZeroExit {
            exit_code, stderr, ..
        }) => {
            let detail = if stderr.is_empty() {
                format!("exit status: {exit_code}")
            } else {
                format!("exit status: {exit_code}: {stderr}")
            };
            // ENOSPC/EACCES/ERANGE stay typed so the caller can classify them.
            Err(match exit_code {
                28 => SwapError::DiskFull,
                13 => SwapError::PermissionDenied,
                34 => SwapError::InvalidSize,
                _ => SwapError::Other(detail),
            })
        }
        Err(e) => Err(SwapError::Other(e.to_string())),
    }
}

/// The kernel's own "a client holds this NBD device" signal (AUD-10).
///
/// `/sys/block/<name>/pid` is non-zero while a client is attached and zero
/// once the connection is gone. An unreadable or missing file is **not**
/// absence: it is an uninspectable device, and ambiguity preserves the
/// resource rather than authorizing a claim that it fell.
pub fn nbd_connection_gone(dev: &str) -> bool {
    nbd_connection_gone_impl(&sysfs_holder_pid_path(dev))
}

fn sysfs_holder_pid_path(dev: &str) -> String {
    let name = dev
        .rsplit('/')
        .next()
        .filter(|n| !n.is_empty())
        .unwrap_or("");
    format!("/sys/block/{name}/pid")
}

fn nbd_connection_gone_impl(path: &str) -> bool {
    matches!(
        std::fs::read_to_string(path)
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok()),
        Some(0)
    )
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SwapError {
    DiskFull,
    PermissionDenied,
    InvalidSize,
    /// The helper was contained at its deadline (AUD-11). Not a refusal: the
    /// kernel may already have taken the effect before the helper was killed.
    Timeout(String),
    Other(String),
}

impl SwapError {
    /// True when the helper was contained rather than refusing. Such a result
    /// is ambiguous about kernel state and must not authorize a rollback.
    pub fn is_timeout(&self) -> bool {
        matches!(self, SwapError::Timeout(_))
    }
}

impl std::fmt::Display for SwapError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SwapError::DiskFull => write!(f, "disk full (ENOSPC)"),
            SwapError::PermissionDenied => write!(f, "permission denied (EACCES)"),
            SwapError::InvalidSize => write!(f, "invalid size (ERANGE)"),
            SwapError::Timeout(e) => write!(f, "{}", e),
            SwapError::Other(e) => write!(f, "{}", e),
        }
    }
}

impl std::error::Error for SwapError {}

/// A failed swap effect that may have left the device in use (AUD-10/AUD-11).
///
/// `retained` is the caller's cleanup authority: `true` means the device may
/// still hold a connection and/or swap and must stay in the ownership map so a
/// later teardown or end-of-session cleanup can still find it. `false` means
/// the effect verifiably left nothing behind.
#[derive(Debug, PartialEq, Eq)]
pub struct SwapFailure {
    pub error: SwapError,
    pub retained: bool,
}

impl std::fmt::Display for SwapFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.error.fmt(f)
    }
}

impl std::error::Error for SwapFailure {}

/// Releases one NBD connection and verifies its absence (AUD-10/AUD-11).
///
/// Success requires verified absence: the disconnect succeeded, or the
/// kernel's own holder pid proves the connection is already gone. Anything
/// else is ambiguous and leaves the decision to the caller's `retained` flag.
fn release_connection<F, P>(
    run_cmd: &mut F,
    connection_gone: &mut P,
    dev: &str,
) -> std::result::Result<(), String>
where
    F: FnMut(&str, &[String], Duration) -> std::result::Result<(), SwapError>,
    P: FnMut(&str) -> bool,
{
    match run_cmd(
        "nbd-client",
        &["-d".to_string(), dev.to_string()],
        EFFECT_TIMEOUT,
    ) {
        Ok(()) => Ok(()),
        Err(_) if connection_gone(dev) => Ok(()),
        Err(e) => Err(e.to_string()),
    }
}

/// Rolls back the connection a failed attach just acquired, and reports both
/// the original cause and the release outcome (AUD-10).
fn release_after_attach<F, P>(
    run_cmd: &mut F,
    connection_gone: &mut P,
    dev: &str,
    step: &str,
    cause: SwapError,
) -> SwapFailure
where
    F: FnMut(&str, &[String], Duration) -> std::result::Result<(), SwapError>,
    P: FnMut(&str) -> bool,
{
    let cause = format!("{step}: {cause}");
    match release_connection(run_cmd, connection_gone, dev) {
        Ok(()) => SwapFailure {
            error: SwapError::Other(format!("{cause}; released the nbd connection")),
            retained: false,
        },
        Err(disconnect) => SwapFailure {
            error: SwapError::Other(format!(
                "{cause}; nbd connection NOT released: {disconnect}"
            )),
            retained: true,
        },
    }
}

/// Full attach sequence (DT-16): `nbd-client` → `mkswap` → `swapon`.
///
/// Every acquired resource is recorded. Once `nbd-client` has connected the
/// device, a later definitive failure releases that connection again before
/// returning: returning early there used to leave a connected NBD device with
/// no owner and no record (AUD-10). A timed-out `swapon` is the exception —
/// see below. On failure the message is what the agent reports via
/// `SwapOnDone{ok:false,detail}` and `SwapFailure::retained` says whether the
/// device is still owed a teardown.
pub fn attach_swap_with<F, P>(
    endpoint: &NbdEndpoint,
    export: &str,
    dev: &str,
    prio: Option<i32>,
    mut run_cmd: F,
    mut connection_gone: P,
) -> std::result::Result<(), SwapFailure>
where
    F: FnMut(&str, &[String], Duration) -> std::result::Result<(), SwapError>,
    P: FnMut(&str) -> bool,
{
    run_cmd(
        "nbd-client",
        &nbd_args(endpoint, export, dev),
        EFFECT_TIMEOUT,
    )
    .map_err(|error| {
        // Nothing was acquired by this call, but a timed-out connect is
        // ambiguous about whether the device is now held: reconcile before
        // claiming the call left nothing behind.
        if error.is_timeout() {
            release_after_attach(&mut run_cmd, &mut connection_gone, dev, "nbd-client", error)
        } else {
            SwapFailure {
                error: SwapError::Other(format!("nbd-client: {error}")),
                retained: false,
            }
        }
    })?;

    // The device is connected from here on: a failure below must release it.
    // DT-16: exported VRAM returns dirty/zeroed; the swap header needs to be
    // rewritten. `mkswap` cannot enable swap, so its failure always rolls back.
    if let Err(error) = run_cmd("mkswap", &[dev.to_string()], EFFECT_TIMEOUT) {
        return Err(release_after_attach(
            &mut run_cmd,
            &mut connection_gone,
            dev,
            "mkswap",
            error,
        ));
    }
    if let Err(error) = run_cmd("swapon", &swapon_args(dev, prio), EFFECT_TIMEOUT) {
        if error.is_timeout() {
            // `swapon` may already have taken effect before the helper was
            // contained. Releasing the connection under live swap would
            // corrupt it: retain the device and say the state is unknown
            // (AUD-11, "preserve swap/device state on timeout").
            return Err(SwapFailure {
                error: SwapError::Other(format!(
                    "swapon: {error}; device retained with unknown swap state ({dev})"
                )),
                retained: true,
            });
        }
        return Err(release_after_attach(
            &mut run_cmd,
            &mut connection_gone,
            dev,
            "swapon",
            error,
        ));
    }
    Ok(())
}

/// Full attach sequence (DT-16): `nbd-client` → `mkswap` → `swapon`.
pub fn attach_swap(
    endpoint: &NbdEndpoint,
    export: &str,
    dev: &str,
    prio: Option<i32>,
) -> std::result::Result<(), SwapFailure> {
    attach_swap_with(endpoint, export, dev, prio, run, nbd_connection_gone)
}

/// Detach sequence: `swapoff` → `nbd-client -d` (AUD-10/AUD-11).
///
/// `swapoff` is mandatory and its failure is fatal — that is the swap-absence
/// claim — and it always retains the device: a failed or timed-out `swapoff`
/// is precisely the ambiguous case where swap may still be on. The disconnect
/// is no longer discarded: if it fails and the kernel still reports a held
/// connection, the detach did not finish. A disconnect that fails while the
/// connection is verifiably gone is the device having already fallen, which
/// is success.
pub fn detach_swap_with<F, P>(
    dev: &str,
    mut run_cmd: F,
    mut connection_gone: P,
) -> std::result::Result<(), SwapFailure>
where
    F: FnMut(&str, &[String], Duration) -> std::result::Result<(), SwapError>,
    P: FnMut(&str) -> bool,
{
    run_cmd("swapoff", &[dev.to_string()], SWAPOFF_TIMEOUT).map_err(|error| SwapFailure {
        error: SwapError::Other(format!("swapoff: {error}")),
        retained: true,
    })?;
    match release_connection(&mut run_cmd, &mut connection_gone, dev) {
        Ok(()) => Ok(()),
        Err(disconnect) => Err(SwapFailure {
            error: SwapError::Other(format!(
                "nbd-client -d: {disconnect}; connection still held ({dev})"
            )),
            retained: true,
        }),
    }
}

/// Detach sequence: `swapoff` → `nbd-client -d`, verified against kernel state.
pub fn detach_swap(dev: &str) -> std::result::Result<(), SwapFailure> {
    detach_swap_with(dev, run, nbd_connection_gone)
}

#[cfg(test)]
mod tests;
