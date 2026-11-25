// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-winsvc/src/vm_identity/sampler.rs
//! Independent native VM observations; no GPU dependency or inline HCS call.
use super::VmIdentityReport;
use crate::native_sampler::{NativeSampler, ProbeKind};
use std::path::PathBuf;
static OWNER: NativeSampler<VmIdentityReport> = NativeSampler::new();
pub fn start(executable: PathBuf) -> Result<(), String> {
    OWNER.start(executable, ProbeKind::Vm)
}
pub fn observation() -> Option<VmIdentityReport> {
    OWNER.observation()
}
pub fn diagnostic() -> Option<String> {
    OWNER.diagnostic()
}
pub fn set_paused(paused: bool) {
    OWNER.set_paused(paused);
}
pub fn stop_and_wait() -> Result<(), String> {
    OWNER.stop_and_wait()
}
