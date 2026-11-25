// SPDX-License-Identifier: GPL-2.0-only
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/args.rs
/*
 * RamShared - daemon argument parsing and supported option combinations
 *
 * Copyright (C) 2026 Emerson Busson
 *
 * Extracted from `main.rs` per `workspace module layout`
 * (AUD-28, step 3: pure argument parsing first). This module has no driver,
 * swap, NBD, ublk, GPU or filesystem side effect: it only decides whether a
 * given argv is a supported combination and which validated shape it has.
 */

use super::native_origin::NativeOriginRequest;
use super::origin_identity::{MAX_ORIGIN_LOGICAL_SIZE, MIN_ORIGIN_LOGICAL_SIZE};
use super::{BLOCK_SIZE, GIB, ORIGIN_MANIFEST_PATH};

/// Transport selected by `--transport`. The parser accepts only these two
/// spellings; anything else is a refusal, not a default.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Transport {
    Nbd,
    Ublk,
}

/// VRAM/tier backend: `Vram` (CUDA, with residency §9/§9.4), `Vulkan`
/// (compatible Vulkan adapters via `ramshared-vulkan`, RF-G2), or `Ram`
/// (without GPU). `Ram` exists to validate the **lifecycle/teardown** of the
/// ublk daemon in **QEMU** (where there is no GPU); the teardown bug that hung
/// WSL2 is independent of the backend. `Vulkan` covers broker + NBD single (generic paths); ublk
/// with Vulkan is deferred (DT-11: the ublk residency server is CUDA-fixed).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum BackendKind {
    Vram,
    Vulkan,
    Ram,
    Auto,
}

impl BackendKind {
    pub(super) fn label(self) -> &'static str {
        match self {
            BackendKind::Vram => "vram",
            BackendKind::Vulkan => "vulkan",
            BackendKind::Ram => "ram",
            BackendKind::Auto => "auto",
        }
    }
}

pub(super) const DEFAULT_SIZE: u64 = 256 * 1024 * 1024;
pub(super) const DEFAULT_ORIGIN_SIZE: u64 = 4 * GIB;

pub(super) struct AppArgs {
    pub(super) size: u64,
    pub(super) origin: Option<String>,
    pub(super) native_origin: Option<NativeOriginRequest>,
    pub(super) sock: String,
    pub(super) force: bool,
    pub(super) nbd_dev: String,
    pub(super) transport: Transport,
    /// Explicit operator acknowledgment for NBD-backed swap on WSL2 after
    /// the verified test case qualification. Travels with the CLI's `up` spawn.
    pub(super) accept_wsl2_nbd: bool,
    /// Serve-pool worker count (SPEC wsl2-nbd-serving-throughput, DT-6):
    /// default 4, bounded 1..=16. `1` reproduces the serialized loop.
    pub(super) serve_threads: usize,
    /// The tier device the spill pump serves (SPEC
    /// zram-writeback-vram-tier DT-8). The zram that is backed by it is
    /// discovered from the kernel, because the daemon starts before the
    /// cascade binds it. `None` keeps the daemon out of the writeback
    /// topology.
    pub(super) zram_pump_tier: Option<String>,
    pub(super) queue_depth: u16,
    pub(super) backend: BackendKind,
    pub(super) slices: u16,
    pub(super) slice_bytes: u64,
    pub(super) listen_nbd_addr: Option<std::net::SocketAddr>,
    pub(super) arbiter_addr: Option<std::net::SocketAddr>,
    pub(super) advertise_tcp: Option<(String, u16)>,
    pub(super) telemetry_jsonl: Option<std::path::PathBuf>,
}

impl AppArgs {
    /// Parses an explicit argv vector before any backend selection or side effect.
    /// Keeping this boundary injectable makes all public refusals testable without
    /// loading CUDA/Vulkan or touching swap, NBD, or ublk state (memory-broker DT-46).
    pub(super) fn parse_from(args: &[String]) -> Result<Self, Box<dyn std::error::Error>> {
        let mut size = DEFAULT_SIZE;
        let mut size_explicit = false;
        let mut origin = None;
        let mut native_origin_path = None;
        let mut sock = "/run/ramshared/wsl2d.sock".to_string();
        let mut force = false;
        let mut accept_wsl2_nbd = false;
        let mut serve_threads = super::serve_pool::DEFAULT_SERVE_THREADS;
        let mut zram_pump_tier: Option<String> = None;
        let mut nbd_dev = "/dev/nbd0".to_string();
        let mut transport = Transport::Nbd;
        let mut queue_depth = 1u16;
        let mut backend = BackendKind::Vram;
        let mut slices = 0u16;
        let mut slice_mb = 0u64;
        let mut listen_nbd: Option<String> = None;
        let mut arbiter: Option<String> = None;
        let mut advertise_nbd: Option<String> = None;
        let mut telemetry_jsonl: Option<String> = None;

        let mut i = 1;
        while i < args.len() {
            match args[i].as_str() {
                "--size" => {
                    i += 1;
                    let mb: u64 = args
                        .get(i)
                        .ok_or("--size requires a value (MiB)")?
                        .parse()?;
                    size = mb
                        .checked_mul(1024 * 1024)
                        .ok_or("--size: MiB value overflow")?;
                    size_explicit = true;
                }
                "--sock" => {
                    i += 1;
                    sock = args.get(i).ok_or("--sock requires a path")?.clone();
                }
                "--origin-manifest" => {
                    i += 1;
                    origin = Some(
                        args.get(i)
                            .ok_or("--origin-manifest requires a path")?
                            .clone(),
                    );
                }
                "--native-origin-manifest" => {
                    i += 1;
                    let path = args
                        .get(i)
                        .ok_or("--native-origin-manifest requires a path")?
                        .clone();
                    if native_origin_path.replace(path).is_some() {
                        return Err("--native-origin-manifest cannot be repeated".into());
                    }
                }
                "--origin" => {
                    return Err("--origin is unsafe; use the sealed --origin-manifest path".into());
                }
                "--force" => force = true,
                "--accept-wsl2-nbd" => accept_wsl2_nbd = true,
                "--serve-threads" => {
                    i += 1;
                    serve_threads = args
                        .get(i)
                        .ok_or("--serve-threads requires a value")?
                        .parse()
                        .map_err(|_| "--serve-threads is invalid")?;
                }
                "--zram-pump-tier" => {
                    i += 1;
                    zram_pump_tier = Some(
                        args.get(i)
                            .ok_or("--zram-pump-tier requires a device path")?
                            .clone(),
                    );
                }
                "--nbd" => {
                    i += 1;
                    nbd_dev = args.get(i).ok_or("--nbd requires a path")?.clone();
                }
                "--transport" => {
                    i += 1;
                    transport = match args.get(i).map(String::as_str) {
                        Some("nbd") => Transport::Nbd,
                        Some("ublk") => Transport::Ublk,
                        _ => return Err("--transport requires 'nbd' or 'ublk'".into()),
                    };
                }
                "--queue-depth" => {
                    i += 1;
                    queue_depth = args
                        .get(i)
                        .ok_or("--queue-depth requires a value")?
                        .parse()
                        .map_err(|_| "--queue-depth is invalid")?;
                }
                "--backend" => {
                    i += 1;
                    backend = match args.get(i).map(String::as_str) {
                        Some("vram") => BackendKind::Vram,
                        Some("vulkan") => BackendKind::Vulkan,
                        Some("ram") => BackendKind::Ram,
                        Some("auto") => BackendKind::Auto,
                        _ => {
                            return Err(
                                "--backend requires 'vram', 'vulkan', 'ram', or 'auto'".into()
                            );
                        }
                    };
                }
                "--slices" => {
                    i += 1;
                    slices = args
                        .get(i)
                        .ok_or("--slices requires a value")?
                        .parse()
                        .map_err(|_| "--slices is invalid")?;
                }
                "--slice-mb" => {
                    i += 1;
                    slice_mb = args
                        .get(i)
                        .ok_or("--slice-mb requires a value (MiB)")?
                        .parse()
                        .map_err(|_| "--slice-mb is invalid")?;
                }
                "--listen-nbd" => {
                    i += 1;
                    listen_nbd = Some(
                        args.get(i)
                            .ok_or("--listen-nbd requires tcp://IP:PORT")?
                            .clone(),
                    );
                }
                "--arbiter-listen" => {
                    i += 1;
                    arbiter = Some(
                        args.get(i)
                            .ok_or("--arbiter-listen requires IP:PORT")?
                            .clone(),
                    );
                }
                "--advertise-nbd" => {
                    i += 1;
                    advertise_nbd = Some(
                        args.get(i)
                            .ok_or("--advertise-nbd requires HOST:PORT")?
                            .clone(),
                    );
                }
                "--telemetry-jsonl" => {
                    i += 1;
                    telemetry_jsonl = Some(
                        args.get(i)
                            .ok_or("--telemetry-jsonl requires a path")?
                            .clone(),
                    );
                }
                other => return Err(format!("unknown argument: {other}").into()),
            }
            i += 1;
        }
        if native_origin_path.is_some()
            && (origin.is_some()
                || slices > 0
                || transport != Transport::Nbd
                || zram_pump_tier.is_some()
                || force)
        {
            return Err(
                "native origin requires resident single-NBD serving; --force, mixed origins and spill-pump posture are refused"
                    .into(),
            );
        }
        if origin.is_some() && !size_explicit {
            size = DEFAULT_ORIGIN_SIZE;
        }
        size -= size % BLOCK_SIZE as u64; // align to the block size
        if origin.is_some() && !(MIN_ORIGIN_LOGICAL_SIZE..=MAX_ORIGIN_LOGICAL_SIZE).contains(&size)
        {
            return Err("origin-cache logical size must be between 1024 and 24576 MiB".into());
        }
        if origin.as_deref().is_some_and(|p| p != ORIGIN_MANIFEST_PATH) {
            return Err(format!(
                "--origin-manifest must use the sealed {ORIGIN_MANIFEST_PATH} path"
            )
            .into());
        }

        if !(1..=super::serve_pool::MAX_SERVE_THREADS).contains(&serve_threads) {
            return Err(format!(
                "--serve-threads must be between 1 and {}",
                super::serve_pool::MAX_SERVE_THREADS
            )
            .into());
        }

        validate_slice_flags(slices, slice_mb, matches!(transport, Transport::Ublk))?;

        let listen_nbd_addr = listen_nbd
            .as_deref()
            .map(parse_private_listen)
            .transpose()?;
        let arbiter_addr = arbiter.as_deref().map(parse_private_listen).transpose()?;
        let advertise_nbd_addr = advertise_nbd
            .as_deref()
            .map(parse_private_listen)
            .transpose()?;

        if advertise_nbd_addr.is_some() && listen_nbd_addr.is_none() {
            return Err(
                "--advertise-nbd requires --listen-nbd (cannot advertise an unserved endpoint)"
                    .into(),
            );
        }

        if slices > 0 && arbiter_addr.is_none() {
            return Err("--slices requires --arbiter-listen IP:PORT (broker control point)".into());
        }
        if slices == 0 && (arbiter_addr.is_some() || listen_nbd_addr.is_some()) {
            return Err("--arbiter-listen/--listen-nbd require --slices N (N > 0)".into());
        }

        let advertise_tcp = advertise_nbd_addr
            .or(listen_nbd_addr)
            .map(|a| (a.ip().to_string(), a.port()));
        let telemetry_jsonl = telemetry_jsonl.map(std::path::PathBuf::from);

        let slice_bytes = if slices > 0 {
            slice_mb
                .checked_mul(1024 * 1024)
                .ok_or("--slice-mb: MiB value overflow")?
        } else {
            0
        };

        Ok(Self {
            size,
            origin,
            native_origin: native_origin_path
                .map(|path| NativeOriginRequest::new(&path, size_explicit.then_some(size)))
                .transpose()?,
            sock,
            force,
            nbd_dev,
            transport,
            accept_wsl2_nbd,
            serve_threads,
            zram_pump_tier,
            queue_depth,
            backend,
            slices,
            slice_bytes,
            listen_nbd_addr,
            arbiter_addr,
            advertise_tcp,
            telemetry_jsonl,
        })
    }
}

pub(super) fn daemon_version_requested(args: &[String]) -> bool {
    matches!(args, [_, flag] if flag == "--version" || flag == "-V" || flag == "version")
}

/// Loopback, RFC1918 IPv4, IPv6 ULA, and the exact Tailscale CGNAT
/// 100.64.0.0/10 range are the documented private listener families (RNF-2).
fn documented_private_listener_ip(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(ip) => {
            let [a, b, _, _] = ip.octets();
            a == 127
                || a == 10
                || (a == 172 && (16..=31).contains(&b))
                || (a == 192 && b == 168)
                || (a == 100 && (64..=127).contains(&b))
        }
        std::net::IpAddr::V6(ip) => ip.is_loopback() || ip.octets()[0] & 0xfe == 0xfc,
    }
}

/// Parses `IP:PORT` (accepts `tcp://`) and permits only loopback, RFC1918
/// IPv4, IPv6 ULA, and the exact Tailscale CGNAT 100.64.0.0/10 range.
pub(super) fn parse_private_listen(s: &str) -> Result<std::net::SocketAddr, String> {
    let raw = s.strip_prefix("tcp://").unwrap_or(s);
    let addr: std::net::SocketAddr = raw
        .parse()
        .map_err(|_| format!("invalid address '{s}' (use IP:PORT)"))?;
    if !documented_private_listener_ip(addr.ip()) {
        return Err(format!(
            "bind on {} refused — RNF-2 permits only loopback, RFC1918, IPv6 ULA, or Tailscale 100.64.0.0/10",
            addr.ip()
        ));
    }
    Ok(addr)
}

/// Validates the combo of slice flags (DT-3: ublk is single-device in WSL2; `--slice-mb` mandatory).
/// Slices ceiling: `StatusReply` embeds `Vec<Slice>+Vec<SliceIo>+Vec<TenantStatus>` in a single
/// JSON line; above ~430 slices it exceeds the protocol's `MAX_LINE_BYTES` (64 KiB) and the other
/// end rejects the line (ADR-0005). 256 gives margins (~38 KiB) and covers any real use case.
pub(super) fn validate_slice_flags(
    slices: u16,
    slice_mb: u64,
    is_ublk: bool,
) -> Result<(), String> {
    if slices > 0 && is_ublk {
        return Err(
            "--slices does not combine with --transport ublk (DT-3: ublk single-device on WSL2)"
                .into(),
        );
    }
    if slices > 0 && slice_mb == 0 {
        return Err("--slices > 0 requires --slice-mb N".into());
    }
    if slices > ramshared_broker::slices::MAX_SLICES {
        return Err(format!(
            "--slices {slices} > {}: StatusReply would exceed the protocol line ceiling \
             (MAX_LINE_BYTES 64 KiB, ADR-0005)",
            ramshared_broker::slices::MAX_SLICES
        ));
    }
    Ok(())
}
