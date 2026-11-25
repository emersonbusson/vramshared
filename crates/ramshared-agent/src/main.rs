// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-agent/src/main.rs
//! `ramshared-agent` — agent (tenant) of the Memory Broker. Connects to the broker via TCP, reports
//! PSI/swaps 1×/s and executes `SwapOn`/`SwapOff`/`DemoteAll` commands over NBD (DT-27).
//!
//! 3-thread architecture with **single writer** (DT-27/R8):
//! - **reader**: blocks on `read_msg(socket)` and forwards each `Msg` to the main loop;
//! - **exec**: executes `attach`/`detach` (blocking) out of the socket path and returns the
//!   result via channel — this way a slow `swapon` never blocks the heartbeat;
//! - **main**: owner of the write socket — sends `Psi`, dispatches commands to exec, drains the
//!   results back as `SwapOnDone`/`SwapOffDone` and arms the watchdog (DT-18).
//!
#![forbid(unsafe_code)]

use std::io::{BufReader, ErrorKind};
use std::time::Duration;

use ramshared_agent::{psi, socket};
use ramshared_broker::protocol::{Msg, read_msg, write_msg};

mod cli;
mod session;

use cli::{CliExit, Config, ParsedArgs, parse_args, usage, usage_diagnostic};
use session::run_agent;
#[cfg(test)]
use session::{
    ExecCmd, ExecResult, INITIAL_BACKOFF, MAX_BACKOFF, SliceRecord, cleanup_owned, commit_result,
    exec_loop, execute_command, handle_msg, next_backoff, reader_loop, session,
};

/// One-shot `--status` I/O deadline (DT-45); prevents a silent broker from hanging the CLI.
const STATUS_IO_TIMEOUT: Duration = Duration::from_secs(5);

fn run(args: &[String]) -> Result<(), CliExit> {
    let cfg = match parse_args(args).map_err(CliExit::Usage)? {
        ParsedArgs::Help => return Err(CliExit::Help),
        ParsedArgs::Config(cfg) => cfg,
    };

    if cfg.status_only {
        return run_status(&cfg).map_err(|error| CliExit::Runtime(error.to_string()));
    }
    if cfg.tenant.is_empty() {
        return Err(CliExit::Usage(format!(
            "--tenant is required in agent mode\n{}",
            usage()
        )));
    }

    // DT-26: swap requires privilege. Reads euid via /proc (no libc) and refuses early, with number.
    let euid = psi::read_euid().map_err(|error| CliExit::Runtime(error.to_string()))?;
    if euid != 0 {
        return Err(CliExit::Runtime(format!(
            "root is required for swap (current euid={euid}, expected 0)"
        )));
    }

    run_agent(&cfg).map_err(|error| CliExit::Runtime(error.to_string()))
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => {}
        Err(CliExit::Help) => println!("{}", usage()),
        Err(CliExit::Usage(message)) => {
            eprintln!("{}", usage_diagnostic(&message));
            std::process::exit(2);
        }
        Err(CliExit::Runtime(message)) => {
            eprintln!("[agent] error: {message}");
            std::process::exit(1);
        }
    }
}

/// `--status` mode: one-shot query (does not register; the broker responds with `StatusReply` to any
/// session) and prints the status.
fn run_status(cfg: &Config) -> Result<(), Box<dyn std::error::Error>> {
    // Absolute connect deadline + armed write side (AUD-11): a blackholed SYN
    // or a non-reading broker must not hang this one-shot query.
    let stream = socket::connect_bounded(&cfg.broker)?;
    stream.set_read_timeout(Some(STATUS_IO_TIMEOUT))?;
    stream.set_write_timeout(Some(STATUS_IO_TIMEOUT))?;
    let mut w = stream.try_clone()?;
    let mut r = BufReader::new(stream);
    write_msg(&mut w, &Msg::Status)?;
    for _ in 0..50 {
        let response = match read_msg(&mut r) {
            Ok(response) => response,
            Err(ramshared_broker::protocol::ProtocolError::ConnectionClosed(error))
                if matches!(error.kind(), ErrorKind::TimedOut | ErrorKind::WouldBlock) =>
            {
                return Err(format!(
                    "broker status timed out after {}s",
                    STATUS_IO_TIMEOUT.as_secs()
                )
                .into());
            }
            Err(error) => return Err(error.into()),
        };
        match response {
            Some(Msg::StatusReply {
                tenants,
                slices,
                slice_io,
                last_rebalance_secs,
            }) => {
                println!("tenants ({}):", tenants.len());
                for t in &tenants {
                    let mark = if t.present { "+" } else { "-" };
                    // A pressure figure is named only when one was measured.
                    // `psi.avg10=0.00` would claim an idle nobody observed.
                    let psi = t
                        .psi
                        .map_or_else(|| "unmeasured".to_string(), |s| format!("{:.2}", s.avg10));
                    println!(
                        "  {mark} id={} name={} slices={:?} psi.avg10={psi}",
                        t.id, t.name, t.slices
                    );
                }
                println!("slices ({}):", slices.len());
                for s in &slices {
                    println!(
                        "  s{} off={} len={} tenant={:?} state={:?}",
                        s.id, s.offset, s.len, s.tenant, s.state
                    );
                }
                println!("slice_io ({}):", slice_io.len());
                for io in &slice_io {
                    println!(
                        "  s{} bytes_served={} io_count={}",
                        io.id, io.bytes_served, io.io_count
                    );
                }
                println!("last_rebalance_secs={last_rebalance_secs:?}");
                return Ok(());
            }
            Some(Msg::Error { reason }) => {
                return Err(format!("broker rejected status: {reason}").into());
            }
            Some(_) => continue,
            None => break,
        }
    }
    Err("broker did not return StatusReply".into())
}

#[cfg(test)]
#[path = "main_tests.rs"]
mod tests;
