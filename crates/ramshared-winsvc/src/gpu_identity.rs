// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-winsvc/src/gpu_identity.rs
//! Bounded native host GPU identity observations; no allocation authority.

#[cfg(windows)]
pub mod process;
#[cfg(windows)]
pub mod sampler;
pub mod supervisor;

#[cfg(windows)]
mod native;
#[cfg(windows)]
pub use native::inspect_native;

pub use ramshared_vram::host_identity::{
    HostGpuIdentity, HostGpuIdentityReport, HostGpuObservation, IdentitySource,
    MAX_IDENTITY_ADAPTERS, MAX_IDENTITY_REPORT_BYTES, ProducerProcess, collect_identities,
    decode_report, encode_report,
};

/// Public read-only command. Native calls execute only in this foreground process.
/// A serving parent must supervise a separate invocation, never call it inline.
pub fn inspect_command() -> i32 {
    #[cfg(windows)]
    let result = print_native_report();
    #[cfg(not(windows))]
    let result: Result<(), String> =
        Err("native host GPU identity inspection requires Windows".into());
    match result {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("GPU identity inspection failed: {error}");
            2
        }
    }
}
#[cfg(windows)]
fn print_native_report() -> Result<(), String> {
    use std::io::Write;
    let encoded = encode_report(&inspect_native()?)?;
    let mut stdout = std::io::stdout().lock();
    stdout
        .write_all(&encoded)
        .and_then(|()| stdout.write_all(b"\n"))
        .map_err(|error| error.to_string())
}
