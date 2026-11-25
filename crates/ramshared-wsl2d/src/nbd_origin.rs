// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/nbd_origin.rs
//! Product NBD execution always retains an opened authoritative origin.
use super::OriginCache;
use super::origin_runtime::OpenedOrigin;
use super::{
    NbdRuntimeStarter, ProductionNbdRuntimeStarter, UnavailableVramProvider, run_nbd_with_startup,
};
use ramshared_block::{AuthoritativeOriginBackend, CacheReleaseOutcome, FileOrigin};
use ramshared_vram::VramProvider;

pub(super) const PRODUCT_ORIGIN_REQUIRED: &str =
    "product NBD requires an opened authoritative origin";

pub(super) fn run_nbd(
    origin: Option<OpenedOrigin>,
    size: u64,
    sock: String,
    force: bool,
    nbd_dev: String,
    serve_threads: usize,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut starter = ProductionNbdRuntimeStarter;
    run_origin_nbd_with_startup(
        UnavailableVramProvider,
        origin,
        size,
        sock,
        force,
        nbd_dev,
        serve_threads,
        &mut starter,
    )
}

#[allow(clippy::too_many_arguments)] // Same injected core is used by real-file origin journeys.
pub(super) fn run_origin_nbd_with_startup<P: VramProvider, S: NbdRuntimeStarter>(
    provider: P,
    origin: Option<OpenedOrigin>,
    size: u64,
    sock: String,
    force: bool,
    nbd_dev: String,
    serve_threads: usize,
    starter: &mut S,
) -> Result<(), Box<dyn std::error::Error>> {
    let origin = origin.ok_or(PRODUCT_ORIGIN_REQUIRED)?;
    run_nbd_with_startup(
        provider,
        Some(origin),
        size,
        sock,
        force,
        nbd_dev,
        false,
        serve_threads,
        starter,
    )
}

/// Map final cache acknowledgement without releasing origin/worker custody.
pub(super) fn release_cache(
    b: &AuthoritativeOriginBackend<FileOrigin, OriginCache>,
) -> Result<(), Box<dyn std::error::Error>> {
    // Same honesty rule as the control-pressure face: the boundary
    // reports what it observed, and it never observes a byte count.
    match b.release_cache() {
        Ok(CacheReleaseOutcome::Revoked) => {
            eprintln!("[ramsharedd] shutdown: released the clean origin cache");
            Ok(())
        }
        Ok(CacheReleaseOutcome::AlreadyReleased) => {
            eprintln!("[ramsharedd] shutdown: clean origin cache was already released");
            Ok(())
        }
        Err(error) => Err(format!("origin cache release was not acknowledged: {}", error.0).into()),
    }
}
