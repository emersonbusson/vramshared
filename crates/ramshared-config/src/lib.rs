// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-config/src/lib.rs
//! Shared TOML configuration schema for broker and agents.
//!
//! Parsing is deliberately separate from process startup so CLI overrides and
//! validation can be tested without sockets, root, or a GPU.
//!
//! Scope: this module owns the **schema and its validation rules**. Runtime
//! startup that selects a backend or opens a listener validates its own
//! arguments on the live path (`ramshared-wsl2d`); see AUD-29 evidence for the
//! startup-invocation proof. Nothing here opens sockets or mutates the host.
#![forbid(unsafe_code)]

pub mod cache_configuration;
pub mod error;
pub mod native_origin;
pub mod resource_profile;
pub use error::ConfigError;

use serde::Deserialize;

/// Maximum slice count accepted by the shared schema.
///
/// Matches `ramshared_broker::slices::MAX_SLICES`. A status reply embeds every
/// slice and its I/O counters in one JSON line; above this ceiling the line
/// exceeds the protocol's `MAX_LINE_BYTES` (64 KiB) and the peer rejects it.
/// The runtime constant is the enforcement point; this one keeps the schema
/// from accepting a value the wire cannot carry.
pub const MAX_SLICES: u16 = 256;

/// Minimum allocation unit for one slice, in MiB.
pub const MIN_SLICE_MIB: u64 = 16;

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub broker: BrokerConfig,
    #[serde(default)]
    pub agent: AgentConfig,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BrokerConfig {
    #[serde(default = "default_listen")]
    pub listen: String,
    #[serde(default = "default_slices")]
    pub slices: u16,
    #[serde(default = "default_slice_mib")]
    pub slice_mib: u64,
    #[serde(default = "default_backend")]
    pub backend: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AgentConfig {
    #[serde(default = "default_broker")]
    pub broker: String,
    #[serde(default)]
    pub tenant: String,
    #[serde(default = "default_watchdog_secs")]
    pub watchdog_secs: u64,
}

fn default_listen() -> String {
    "127.0.0.1:7777".into()
}
fn default_broker() -> String {
    "127.0.0.1:7777".into()
}
fn default_slices() -> u16 {
    1
}
fn default_slice_mib() -> u64 {
    256
}
fn default_backend() -> String {
    "cuda".into()
}
fn default_watchdog_secs() -> u64 {
    90
}

impl Default for BrokerConfig {
    fn default() -> Self {
        Self {
            listen: default_listen(),
            slices: default_slices(),
            slice_mib: default_slice_mib(),
            backend: default_backend(),
        }
    }
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            broker: default_broker(),
            tenant: String::new(),
            watchdog_secs: default_watchdog_secs(),
        }
    }
}

/// Reads `MemTotal` from a platform meminfo document and returns bytes.
///
/// A missing, unparseable, or absent `MemTotal` is a refusal, not a silent
/// pass: a capacity claim cannot be authorized without a host RAM observation.
pub fn host_ram_from_meminfo(text: &str) -> Result<u64, ConfigError> {
    for line in text.lines() {
        if !line.starts_with("MemTotal:") {
            continue;
        }
        let mut parts = line.split_whitespace();
        let _ = parts.next(); // MemTotal:
        let kb_str = parts.next().ok_or_else(|| ConfigError::Invalid {
            key_path: "platform.meminfo".into(),
            reason: "MemTotal line carries no value".into(),
        })?;
        let kb = kb_str.parse::<u64>().map_err(|_| ConfigError::Invalid {
            key_path: "platform.meminfo".into(),
            reason: format!("MemTotal value '{kb_str}' is not an integer"),
        })?;
        return kb.checked_mul(1024).ok_or_else(|| {
            ConfigError::OutOfRange(format!("MemTotal {kb} kB overflows a byte count"))
        });
    }
    Err(ConfigError::Invalid {
        key_path: "platform.meminfo".into(),
        reason: "MemTotal is required to authorize a capacity claim".into(),
    })
}

/// Reads host RAM from the platform memory facility.
///
/// Linux exposes `/proc/meminfo`. On a platform without that facility the
/// observation is absent and the caller must not authorize a capacity claim.
pub fn host_ram_bytes() -> Result<u64, ConfigError> {
    let text = std::fs::read_to_string("/proc/meminfo").map_err(|err| ConfigError::Invalid {
        key_path: "platform.meminfo".into(),
        reason: format!("host RAM observation unavailable: {err}"),
    })?;
    host_ram_from_meminfo(&text)
}

/// Authorizes a `IP:PORT` listener or peer endpoint.
///
/// Refuses malformed addresses and unspecified binds (`0.0.0.0`, `::`). The
/// check is independent of the selected backend so an unsafe endpoint cannot
/// hide behind a valid backend string.
fn validate_endpoint(key_path: &str, value: &str) -> Result<(), ConfigError> {
    let raw = value.strip_prefix("tcp://").unwrap_or(value);
    let addr: std::net::SocketAddr = raw.parse().map_err(|_| ConfigError::Invalid {
        key_path: key_path.into(),
        reason: format!("invalid endpoint '{value}' (expected IP:PORT)"),
    })?;
    if addr.ip().is_unspecified() {
        return Err(ConfigError::Invalid {
            key_path: key_path.into(),
            reason: format!(
                "endpoint '{value}' binds an unspecified address; use loopback or an explicit private address"
            ),
        });
    }
    Ok(())
}

impl Config {
    pub fn parse(text: &str) -> Result<Self, ConfigError> {
        let deserializer = match toml::Deserializer::parse(text) {
            Ok(d) => d,
            Err(err) => {
                let message = err.message().to_string();
                let mut line = None;
                let mut column = None;
                if let Some(span) = err.span() {
                    let mut l = 1;
                    let mut c = 1;
                    for (i, ch) in text.chars().enumerate() {
                        if i == span.start {
                            line = Some(l);
                            column = Some(c);
                            break;
                        }
                        if ch == '\n' {
                            l += 1;
                            c = 1;
                        } else {
                            c += 1;
                        }
                    }
                }
                return Err(ConfigError::Parse {
                    message,
                    line,
                    column,
                    key_path: String::new(),
                });
            }
        };

        serde_path_to_error::deserialize(deserializer).map_err(|err| {
            let inner_err = err.inner();
            let message = inner_err.message().to_string();
            let mut line = None;
            let mut column = None;
            if let Some(span) = inner_err.span() {
                let mut l = 1;
                let mut c = 1;
                for (i, ch) in text.chars().enumerate() {
                    if i == span.start {
                        line = Some(l);
                        column = Some(c);
                        break;
                    }
                    if ch == '\n' {
                        l += 1;
                        c = 1;
                    } else {
                        c += 1;
                    }
                }
            }

            let key_path = err.path().to_string();
            ConfigError::Parse {
                message,
                line,
                column,
                key_path,
            }
        })
    }

    /// Full validation against the live platform RAM observation.
    ///
    /// Fails closed when the observation is unavailable. Use
    /// [`Config::validate_with_host_ram`] when the observation comes from
    /// another facility.
    pub fn validate(&self) -> Result<(), ConfigError> {
        self.validate_with_host_ram(host_ram_bytes()?)
    }

    /// Same rules against an observed host RAM value in bytes.
    ///
    /// This is the generic entry point: a caller that observes memory through
    /// another facility reuses these bounds instead of forking them.
    pub fn validate_with_host_ram(&self, host_ram_bytes: u64) -> Result<(), ConfigError> {
        if self.broker.slices == 0 {
            return Err(ConfigError::Invalid {
                key_path: "broker.slices".into(),
                reason: "must be > 0".into(),
            });
        }
        if self.broker.slices > MAX_SLICES {
            return Err(ConfigError::OutOfRange(format!(
                "broker.slices must be <= {MAX_SLICES} (got {})",
                self.broker.slices
            )));
        }
        if self.broker.slice_mib == 0 {
            return Err(ConfigError::Invalid {
                key_path: "broker.slice_mib".into(),
                reason: "must be > 0".into(),
            });
        }
        if self.broker.slice_mib < MIN_SLICE_MIB {
            return Err(ConfigError::OutOfRange(format!(
                "broker.slice_mib must be >= {MIN_SLICE_MIB} (got {})",
                self.broker.slice_mib
            )));
        }
        if self.agent.watchdog_secs == 0 {
            return Err(ConfigError::Invalid {
                key_path: "agent.watchdog_secs".into(),
                reason: "must be > 0".into(),
            });
        }
        // Structural duration bound: a watchdog expressed in milliseconds must
        // not overflow the derived count. No calibrated product ceiling is
        // invented here.
        if self.agent.watchdog_secs.checked_mul(1000).is_none() {
            return Err(ConfigError::OutOfRange(format!(
                "agent.watchdog_secs {} overflows a millisecond count",
                self.agent.watchdog_secs
            )));
        }

        validate_endpoint("broker.listen", &self.broker.listen)?;
        validate_endpoint("agent.broker", &self.agent.broker)?;

        match self.broker.backend.as_str() {
            "cuda" | "vulkan" => {}
            other => {
                return Err(ConfigError::Invalid {
                    key_path: "broker.backend".into(),
                    reason: format!("unsupported backend: {other}"),
                });
            }
        }

        // Checked, not saturating: a product that wraps silently would under-
        // report capacity and pass the ceiling below.
        let total_mib = u64::from(self.broker.slices)
            .checked_mul(self.broker.slice_mib)
            .ok_or_else(|| {
                ConfigError::OutOfRange(format!(
                    "broker.slices {} * broker.slice_mib {} overflows a MiB count",
                    self.broker.slices, self.broker.slice_mib
                ))
            })?;
        let total_bytes = total_mib.checked_mul(1024 * 1024).ok_or_else(|| {
            ConfigError::OutOfRange(format!(
                "configured capacity {total_mib} MiB overflows a byte count"
            ))
        })?;

        if total_bytes > host_ram_bytes {
            return Err(ConfigError::OutOfRange(format!(
                "configured memory ({total_bytes} bytes) exceeds host RAM ({host_ram_bytes} bytes)"
            )));
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]
    use super::*;

    /// Observation large enough for any default or small explicit config, and
    /// small enough that a 1000 x 1 MiB claim is refused.
    const HOST_RAM: u64 = 4 * 1024 * 1024 * 1024;

    #[test]
    fn defaults_are_safe_and_local() {
        let cfg = Config::parse("").expect("parse");
        assert_eq!(cfg.broker.listen, "127.0.0.1:7777");
        assert_eq!(cfg.broker.backend, "cuda");
        assert!(cfg.validate_with_host_ram(HOST_RAM).is_ok());
    }

    #[test]
    fn parses_explicit_config() {
        let cfg = Config::parse("[broker]\nlisten='127.0.0.1:8888'\nslices=4\nslice_mib=128\nbackend='vulkan'\n[agent]\ntenant='dcc'\n").expect("parse");
        assert_eq!(cfg.broker.slices, 4);
        assert_eq!(cfg.agent.tenant, "dcc");
        assert!(cfg.validate_with_host_ram(HOST_RAM).is_ok());
    }

    #[test]
    fn rejects_unsafe_or_invalid_configuration() {
        let mut cfg = Config::parse("").expect("parse");
        cfg.broker.listen = "0.0.0.0:7777".into();
        cfg.broker.backend = "unknown".into();
        let err = cfg
            .validate_with_host_ram(HOST_RAM)
            .expect_err("expected error");
        assert!(matches!(
            err,
            ConfigError::Invalid {
                ref key_path,
                ref reason,
            } if key_path == "broker.listen" && reason.contains("unspecified")
        ));
    }

    #[test]
    fn parses_error_captures_location() {
        let err = Config::parse("[broker]\nslice_mib = 'hello'").expect_err("expected error");
        assert!(matches!(
            err,
            ConfigError::Parse {
                line: Some(2),
                column: Some(13),
                ref key_path,
                ..
            } if key_path == "broker.slice_mib"
        ));
    }

    #[test]
    fn display_errors() {
        let p1 = ConfigError::Parse {
            message: "msg".into(),
            line: Some(1),
            column: Some(2),
            key_path: "broker".into(),
        };
        assert_eq!(
            p1.to_string(),
            "parse error at line 1, col 2 for key 'broker': msg"
        );

        let p2 = ConfigError::Parse {
            message: "msg".into(),
            line: None,
            column: None,
            key_path: "".into(),
        };
        assert_eq!(p2.to_string(), "parse error: msg");

        let i = ConfigError::Invalid {
            key_path: "k".into(),
            reason: "r".into(),
        };
        assert_eq!(i.to_string(), "invalid configuration at 'k': r");
    }

    #[test]
    fn rejects_zero_slices() {
        let mut cfg = Config::parse("").expect("parse");
        cfg.broker.slices = 0;
        let err = cfg
            .validate_with_host_ram(HOST_RAM)
            .expect_err("should reject zero slices");
        assert!(matches!(
            err,
            ConfigError::Invalid {
                ref key_path,
                ref reason,
            } if key_path == "broker.slices" && reason == "must be > 0"
        ));
    }

    #[test]
    fn rejects_zero_slice_mib() {
        let mut cfg = Config::parse("").expect("parse");
        cfg.broker.slice_mib = 0;
        let err = cfg
            .validate_with_host_ram(HOST_RAM)
            .expect_err("should reject zero slice_mib");
        assert!(matches!(
            err,
            ConfigError::Invalid {
                ref key_path,
                ref reason,
            } if key_path == "broker.slice_mib" && reason == "must be > 0"
        ));
    }

    #[test]
    fn rejects_zero_watchdog_secs() {
        let mut cfg = Config::parse("").expect("parse");
        cfg.agent.watchdog_secs = 0;
        let err = cfg
            .validate_with_host_ram(HOST_RAM)
            .expect_err("should reject zero watchdog_secs");
        assert!(matches!(
            err,
            ConfigError::Invalid {
                ref key_path,
                ref reason,
            } if key_path == "agent.watchdog_secs" && reason == "must be > 0"
        ));
    }

    #[test]
    fn accepts_vulkan_backend() {
        let mut cfg = Config::parse("").expect("parse");
        cfg.broker.backend = "vulkan".into();
        assert!(cfg.validate_with_host_ram(HOST_RAM).is_ok());
    }

    #[test]
    fn rejects_small_slice() {
        let mut cfg = Config::parse("").expect("parse");
        cfg.broker.slice_mib = MIN_SLICE_MIB - 1;
        let err = cfg
            .validate_with_host_ram(HOST_RAM)
            .expect_err("should reject small slice");
        assert!(matches!(err, ConfigError::OutOfRange(_)));
    }

    #[test]
    fn rejects_excessive_ram() {
        let mut cfg = Config::parse("").expect("parse");
        cfg.broker.slices = MAX_SLICES;
        cfg.broker.slice_mib = 1024 * 1024; // 1 PiB total
        let err = cfg
            .validate_with_host_ram(HOST_RAM)
            .expect_err("should reject excessive ram");
        assert!(matches!(err, ConfigError::OutOfRange(_)));
        assert!(
            err.to_string().contains("exceeds host RAM"),
            "expected a capacity refusal, got: {err}"
        );
    }

    #[test]
    fn parses_syntax_error_captures_line_col() {
        let err = Config::parse("[broker]\nlisten = '127.0.0.1:7777'\ninvalid = = syntax")
            .expect_err("expected syntax error");
        assert!(matches!(
            err,
            ConfigError::Parse {
                line: Some(3),
                column: Some(11),
                ref key_path,
                ..
            } if key_path.is_empty()
        ));
    }

    #[test]
    fn parses_type_mismatch_in_agent_captures_key_path_and_location() {
        let err = Config::parse("[agent]\nwatchdog_secs = 'not_a_number'")
            .expect_err("expected parse error");
        assert!(matches!(
            err,
            ConfigError::Parse {
                line: Some(2),
                column: Some(17),
                ref key_path,
                ..
            } if key_path == "agent.watchdog_secs"
        ));
    }

    #[test]
    fn parses_partial_agent_config_and_applies_defaults() {
        let cfg = Config::parse("[agent]\ntenant = 'my-tenant'").expect("parse partial config");
        assert_eq!(cfg.agent.tenant, "my-tenant");
        assert_eq!(cfg.agent.broker, "127.0.0.1:7777");
        assert_eq!(cfg.agent.watchdog_secs, 90);
        assert_eq!(cfg.broker.listen, "127.0.0.1:7777");
        assert_eq!(cfg.broker.slices, 1);
        assert_eq!(cfg.broker.slice_mib, 256);
        assert_eq!(cfg.broker.backend, "cuda");
    }

    // --- AUD-29 required cases -------------------------------------------

    #[test]
    fn rejects_misspelled_field() {
        let err = Config::parse("[broker]\nlstten = '127.0.0.1:7777'\n")
            .expect_err("misspelled field must not be ignored");
        assert!(matches!(err, ConfigError::Parse { .. }));

        let err = Config::parse("[agent]\nwatchdog_sec = 5\n")
            .expect_err("misspelled agent field must not be ignored");
        assert!(matches!(err, ConfigError::Parse { .. }));

        let err = Config::parse("brokr = {}\n")
            .expect_err("misspelled top-level table must not be ignored");
        assert!(matches!(err, ConfigError::Parse { .. }));
    }

    #[test]
    fn rejects_malformed_endpoint_with_valid_backend() {
        for (key_path, toml) in [
            (
                "broker.listen",
                "[broker]\nlisten = 'not-an-address'\nbackend = 'cuda'\n",
            ),
            (
                "broker.listen",
                "[broker]\nlisten = '127.0.0.1'\nbackend = 'cuda'\n",
            ),
            (
                "agent.broker",
                "[agent]\nbroker = 'not-an-address'\n[broker]\nbackend = 'cuda'\n",
            ),
        ] {
            let cfg = Config::parse(toml).expect("schema-valid document");
            let err = cfg
                .validate_with_host_ram(HOST_RAM)
                .expect_err("malformed endpoint must be refused");
            assert!(
                matches!(&err, ConfigError::Invalid { key_path: k, reason } if k == key_path && reason.contains("invalid endpoint")),
                "unexpected error for {toml:?}: {err}"
            );
        }
    }

    #[test]
    fn rejects_unspecified_endpoint_with_valid_backend() {
        for (key_path, toml) in [
            (
                "broker.listen",
                "[broker]\nlisten = '0.0.0.0:7777'\nbackend = 'cuda'\n",
            ),
            (
                "broker.listen",
                "[broker]\nlisten = 'tcp://[::]:7777'\nbackend = 'vulkan'\n",
            ),
            (
                "agent.broker",
                "[agent]\nbroker = '0.0.0.0:7777'\n[broker]\nbackend = 'cuda'\n",
            ),
        ] {
            let cfg = Config::parse(toml).expect("schema-valid document");
            let err = cfg
                .validate_with_host_ram(HOST_RAM)
                .expect_err("unspecified endpoint must be refused");
            assert!(
                matches!(&err, ConfigError::Invalid { key_path: k, reason } if k == key_path && reason.contains("unspecified")),
                "unexpected error for {toml:?}: {err}"
            );
        }
    }

    #[test]
    fn accepts_non_loopback_private_endpoint() {
        let cfg = Config::parse("[broker]\nlisten = '192.168.1.10:7777'\nbackend = 'cuda'\n")
            .expect("parse");
        assert!(cfg.validate_with_host_ram(HOST_RAM).is_ok());
    }

    #[test]
    fn rejects_maximum_slices() {
        let mut cfg = Config::parse("").expect("parse");
        cfg.broker.slices = MAX_SLICES;
        cfg.broker.slice_mib = MIN_SLICE_MIB;
        assert!(
            cfg.validate_with_host_ram(HOST_RAM).is_ok(),
            "MAX_SLICES must remain acceptable"
        );

        cfg.broker.slices = MAX_SLICES.saturating_add(1);
        let err = cfg
            .validate_with_host_ram(HOST_RAM)
            .expect_err("slices above the protocol ceiling must be refused");
        assert!(matches!(err, ConfigError::OutOfRange(_)));
    }

    #[test]
    fn refuses_slice_capacity_multiplication_overflow() {
        let mut cfg = Config::parse("").expect("parse");
        cfg.broker.slices = u16::MAX;
        cfg.broker.slice_mib = u64::MAX;
        let err = cfg
            .validate_with_host_ram(u64::MAX)
            .expect_err("slices * slice_mib must not wrap");
        assert!(matches!(err, ConfigError::OutOfRange(_)));
    }

    #[test]
    fn refuses_watchdog_duration_overflow() {
        let mut cfg = Config::parse("").expect("parse");
        cfg.agent.watchdog_secs = u64::MAX;
        let err = cfg
            .validate_with_host_ram(HOST_RAM)
            .expect_err("watchdog duration must not overflow its derived count");
        assert!(matches!(err, ConfigError::OutOfRange(_)));
    }

    #[test]
    fn refuses_missing_platform_telemetry() {
        assert!(host_ram_from_meminfo("").is_err());
        assert!(host_ram_from_meminfo("SwapTotal: 1024 kB\n").is_err());
        assert!(host_ram_from_meminfo("MemTotal: not-a-number kB\n").is_err());
        assert!(host_ram_from_meminfo("MemTotal:\n").is_err());

        let cfg = Config::parse("").expect("parse");
        let err = cfg
            .validate_with_host_ram(0)
            .expect_err("a zero observation cannot authorize a capacity claim");
        assert!(matches!(err, ConfigError::OutOfRange(_)));

        // The live entry point propagates a missing observation as a refusal
        // rather than silently skipping the capacity ceiling.
        assert!(matches!(
            host_ram_from_meminfo("MemTotal: 1 kB\n"),
            Ok(1024)
        ));
    }

    #[test]
    fn host_ram_from_meminfo_returns_bytes() {
        assert_eq!(
            host_ram_from_meminfo("MemTotal:       16384 kB\nMemFree: 1 kB\n").expect("bytes"),
            16384 * 1024
        );
    }
}
