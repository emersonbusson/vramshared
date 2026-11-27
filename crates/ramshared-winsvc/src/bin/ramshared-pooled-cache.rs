// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-winsvc/src/bin/ramshared-pooled-cache.rs
//! Standalone Windows/host pooled VRAM read-cache server binary (`RSCP` v1 over `AF_HYPERV`).

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use ramshared_winsvc::pooled_cache::server::{
    DEFAULT_POOLED_CONFIG_PATH, PooledServerConfig, run_hyperv_server,
};

fn parse_args() -> Result<(PathBuf, Option<PathBuf>), String> {
    let mut args = std::env::args().skip(1);
    let mut config = PathBuf::from(DEFAULT_POOLED_CONFIG_PATH);
    let mut stop_file: Option<PathBuf> = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--config" => {
                let val = args
                    .next()
                    .ok_or_else(|| "--config requires a path".to_string())?;
                config = PathBuf::from(val);
            }
            "--stop-file" => {
                let val = args
                    .next()
                    .ok_or_else(|| "--stop-file requires a path".to_string())?;
                stop_file = Some(PathBuf::from(val));
            }
            "--version" | "-V" => {
                println!("ramshared-pooled-cache {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    Ok((config, stop_file))
}

fn main() -> ExitCode {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    if raw.len() == 2 && raw[0] == "inspect-vms" && raw[1] == "--json" {
        return ExitCode::from(ramshared_winsvc::vm_identity::inspect_command() as u8);
    }
    let (config_path, stop_file) = match parse_args() {
        Ok(v) => v,
        Err(err) => {
            eprintln!("[ramshared-pooled-cache] {err}");
            return ExitCode::from(2);
        }
    };
    let cfg = match PooledServerConfig::load(&config_path) {
        Ok(c) => c,
        Err(err) => {
            eprintln!("[ramshared-pooled-cache] config error: {err}");
            return ExitCode::from(1);
        }
    };
    let stop = Arc::new(AtomicBool::new(false));
    if let Some(sf) = stop_file {
        let stop_watch = Arc::clone(&stop);
        let _ = std::thread::Builder::new()
            .name("pooled-stop-watch".into())
            .spawn(move || {
                while !stop_watch.load(Ordering::Acquire) {
                    if sf.exists() {
                        stop_watch.store(true, Ordering::Release);
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
            });
    }
    match run_hyperv_server(&cfg, stop) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("[ramshared-pooled-cache] runtime error: {err}");
            ExitCode::from(1)
        }
    }
}
