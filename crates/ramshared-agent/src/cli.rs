// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-agent/src/cli.rs
//! CLI parsing and usage diagnostics for `ramshared-agent`.

use std::time::Duration;

use ramshared_broker::model::TransportKind;

pub(super) struct Config {
    pub(super) broker: String,
    pub(super) tenant: String,
    pub(super) swap_prio: Option<i32>,
    pub(super) nbd_base: String,
    pub(super) transport: TransportKind,
    pub(super) watchdog: Duration,
    pub(super) status_only: bool,
}

pub(super) enum ParsedArgs {
    Help,
    Config(Config),
}

#[derive(Debug)]
pub(super) enum CliExit {
    Help,
    Usage(String),
    Runtime(String),
}

pub(super) fn usage() -> String {
    "Usage:\n  \
     ramshared-agent --broker HOST:PORT --tenant NAME [--swap-prio P] \
     [--nbd-base /dev/nbd] [--transport tcp|unix] [--watchdog-secs 90]\n  \
     ramshared-agent --broker HOST:PORT --status"
        .to_string()
}

pub(super) fn usage_diagnostic(message: &str) -> String {
    let usage = usage();
    if message.ends_with(&usage) {
        message.to_string()
    } else {
        format!("{message}\n{usage}")
    }
}

pub(super) fn parse_args(args: &[String]) -> Result<ParsedArgs, String> {
    let mut broker = None;
    let mut tenant = None;
    let mut swap_prio = None;
    let mut nbd_base = "/dev/nbd".to_string();
    let mut transport = TransportKind::NbdTcp;
    let mut watchdog = Duration::from_secs(90);
    let mut status_only = false;

    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let mut take = |name: &str| -> Result<String, String> {
            it.next()
                .cloned()
                .ok_or_else(|| format!("{name} requires a value"))
        };
        match arg.as_str() {
            "--broker" => broker = Some(take("--broker")?),
            "--tenant" => tenant = Some(take("--tenant")?),
            "--swap-prio" => {
                let v = take("--swap-prio")?;
                swap_prio = Some(
                    v.parse()
                        .map_err(|_| format!("--swap-prio is invalid: {v}"))?,
                );
            }
            "--nbd-base" => nbd_base = take("--nbd-base")?,
            "--transport" => {
                transport = match take("--transport")?.as_str() {
                    "tcp" => TransportKind::NbdTcp,
                    "unix" => TransportKind::NbdUnix,
                    other => return Err(format!("--transport is invalid: {other} (use tcp|unix)")),
                };
            }
            "--watchdog-secs" => {
                let v = take("--watchdog-secs")?;
                let s: u64 = v
                    .parse()
                    .map_err(|_| format!("--watchdog-secs is invalid: {v}"))?;
                watchdog = Duration::from_secs(s);
            }
            "--status" => status_only = true,
            "-h" | "--help" => return Ok(ParsedArgs::Help),
            other => return Err(format!("unknown argument: {other}\n{}", usage())),
        }
    }

    Ok(ParsedArgs::Config(Config {
        broker: broker.ok_or_else(|| format!("--broker is required\n{}", usage()))?,
        tenant: tenant.unwrap_or_default(),
        swap_prio,
        nbd_base,
        transport,
        watchdog,
        status_only,
    }))
}
