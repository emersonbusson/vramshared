// SPDX-License-Identifier: MIT
// Copyright (c) 2024-2026 Emerson Busson. All rights reserved.
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/usage.rs
//! Advertised operator surface of the `ramshared` CLI.
//!
//! `write_usage` is the only place the help text is written. The [`Surface`]
//! catalog is the executable statement of that same surface: one entry per
//! verb, naming the flags its parser reads and the flags it matches only to
//! refuse. The synopses stay hand-written because they carry argument shape and
//! prose that bare flag names cannot; the catalog is what stops those two
//! descriptions from drifting apart from the parsers themselves.
//!
//! The tests close `parser ⊆ catalog ⊆ usage ⊆ catalog`:
//!
//! - [`tests::usage_text_names_every_advertised_flag`] — catalog ⊆ usage
//! - [`tests::usage_never_advertises_an_unparsed_flag`] — usage ⊆ catalog
//! - [`tests::parser_match_arms_are_advertised`] — parser ⊆ catalog
//! - [`tests::every_accepted_flag_is_accepted_by_its_parser`] — catalog ⊆ parser
//!
//! A flag added to a parser without a catalog entry fails the third test; a
//! flag written into the help text that no parser reads fails the second. The
//! operator guide is held to the same catalog by
//! [`tests::operator_guide_examples_use_only_advertised_flags`], so a runbook
//! example cannot invent an option the product refuses.

use std::io::Write;

/// One operator-facing verb and the flags its parser reads.
#[cfg(test)]
struct Surface {
    /// Command word as it appears in the synopsis (`"stress|bench"` for an alias).
    command: &'static str,
    /// Bare `--flag` names the parser accepts. Order is usage order.
    accepts: &'static [&'static str],
    /// Bare `--flag` names the parser matches only to refuse them. They are
    /// named in the help text so an operator learns why the flag is closed,
    /// and they never appear as usable options in the synopsis.
    refuses: &'static [&'static str],
}

/// Every flag a `ramshared` parser reads, by command word.
///
/// This is the advertised surface in structured form. The help text is the
/// operator-facing rendering of it; the tests pin both to the parsers. Adding a
/// flag to a parser and not here is a bug the reverse-direction test reports.
#[cfg(test)]
const SURFACES: &[Surface] = &[
    Surface {
        command: "run",
        accepts: &["--class", "--memory-max"],
        refuses: &[],
    },
    Surface {
        command: "session",
        accepts: &["--class", "--memory-max"],
        refuses: &[],
    },
    Surface {
        command: "supervise",
        accepts: &["--once"],
        refuses: &[],
    },
    Surface {
        command: "recover",
        accepts: &["--status", "--resume", "--nbd-peer"],
        refuses: &[],
    },
    Surface {
        command: "check",
        accepts: &["--json"],
        refuses: &[],
    },
    Surface {
        command: "doctor",
        accepts: &["--json"],
        refuses: &[],
    },
    Surface {
        command: "config",
        accepts: &[
            "--json",
            "--profile",
            "--output",
            "--plan",
            "--confirm",
            "--recover",
            "--use-case",
            "--batch",
        ],
        refuses: &[],
    },
    Surface {
        command: "diagnose",
        accepts: &["--events", "--json", "--output"],
        refuses: &[],
    },
    Surface {
        command: "setup",
        accepts: &[
            "--profile",
            "--output",
            "--scope",
            "--plan",
            "--approval",
            "--json",
        ],
        refuses: &[],
    },
    Surface {
        command: "service",
        accepts: &["--approval", "--json"],
        refuses: &[],
    },
    Surface {
        command: "install",
        accepts: &[
            "--plan",
            "--approve-nbd-product-install",
            "--approve-manifest-sha256",
            "--lower-sink",
            "--posture",
            "--enroll-default",
            "--no-enroll",
            "--no-activate",
            "--bundle-root",
            "--target-release",
            "--running",
            "--previous-bundle",
            "--keyring",
            "--policy",
            "--approve-legacy-unit-replacement",
            "--approve-legacy-auxiliary-unit-replacement",
            "--auto-update",
            "--check",
            "--status",
            "--reconcile-rollback",
            "--reconcile-complete",
            "--json",
        ],
        refuses: &[],
    },
    Surface {
        command: "package",
        accepts: &["--action", "--json"],
        refuses: &[],
    },
    Surface {
        command: "up",
        accepts: &[
            "--vram",
            "--zram",
            "--zram-backing-dev",
            "--zram-writeback-tier",
            "--accept-wsl2-nbd",
            "--daemon",
            "--connections",
            "--transport",
            "--swap-dev",
            "--nbd",
            "--force-no-safety-net",
            "--plan",
            "--json",
        ],
        refuses: &["--origin", "--origin-manifest"],
    },
    Surface {
        command: "migrate-cascade",
        accepts: &["--from-legacy"],
        refuses: &[],
    },
    Surface {
        command: "ensure-origin",
        accepts: &[],
        refuses: &[],
    },
    Surface {
        command: "ensure-guardian",
        accepts: &[],
        refuses: &[],
    },
    Surface {
        command: "boot",
        accepts: &[],
        refuses: &[],
    },
    Surface {
        command: "status",
        accepts: &["--json"],
        refuses: &[],
    },
    Surface {
        command: "top",
        accepts: &[
            "--jsonl",
            "--compact",
            "--once",
            "--interval-ms",
            "--history-seconds",
            "--output",
            "--heartbeat",
            "--benchmark-evidence",
            "--lang",
        ],
        refuses: &[],
    },
    Surface {
        command: "monitor",
        accepts: &[
            "--jsonl",
            "--compact",
            "--once",
            "--interval-ms",
            "--history-seconds",
            "--output",
            "--heartbeat",
            "--benchmark-evidence",
            "--lang",
        ],
        refuses: &[],
    },
    // `bench` is a spelling alias of `stress`; the parser matches both words
    // against one option surface, so the catalog records that surface once.
    Surface {
        command: "stress|bench",
        accepts: &[
            "--start",
            "--target",
            "--step",
            "--interval-ms",
            "--hold-sec",
            "--duration-sec",
            "--min-ram-mb",
            "--threads",
            "--battery",
            "--cascade",
            "--full-three-tier",
            "--tier3-only",
            "--tier1-target-pct",
            "--tier2-target-pct",
            "--tier3-target-pct",
            "--physical-cache-target-mib",
            "--max-psi-full",
            "--max-latency-ms",
            "--min-order-7-chunks",
            "--log",
            "--json",
        ],
        refuses: &[],
    },
    Surface {
        command: "down",
        accepts: &["--plan", "--json", "--recover-released-peer"],
        refuses: &[],
    },
    Surface {
        command: "swap-rebalance",
        accepts: &["--plan", "--apply", "--target", "--json"],
        refuses: &["--force", "--all", "--yes"],
    },
    Surface {
        command: "release-sync",
        accepts: &[],
        refuses: &[],
    },
    Surface {
        command: "--version",
        accepts: &["--version", "--build-info", "--help"],
        refuses: &[],
    },
];

/// Top-level spellings the parser reads that are not a `Surface::command`.
#[cfg(test)]
const TOP_LEVEL_FLAGS: &[&str] = &["--version", "--build-info", "--help"];

fn line(stderr: &mut dyn Write, text: &str) {
    let _ = writeln!(stderr, "{text}");
}

/// Render the operator-facing usage text.
///
/// Every flag named here is in [`SURFACES`] or in [`TOP_LEVEL_FLAGS`]; the
/// tests refuse a `--flag` token in this text that no parser reads.
pub(crate) fn write_usage(stderr: &mut dyn Write) {
    line(stderr, "usage:");
    line(stderr, "  ramshared --version | --build-info | --help");
    line(
        stderr,
        "  ramshared run --class <interactive|build|browser-test|batch> [--memory-max MiB] -- <command> [args...]",
    );
    line(
        stderr,
        "  ramshared session --class <class> [--memory-max MiB]",
    );
    line(
        stderr,
        "    Workload admission for run and session requires root.",
    );
    line(stderr, "  ramshared supervise [--once]");
    line(stderr, "  ramshared recover --status|--resume|--nbd-peer");
    line(stderr, "  ramshared check [--json]");
    line(stderr, "  ramshared doctor [--json]");
    line(
        stderr,
        "  ramshared config [--profile PATH | show [--json] | wizard [--json] | plan [--json] [--profile PATH] | draft --output PATH]",
    );
    line(
        stderr,
        "  ramshared config benchmark [--plan | --confirm | --recover] [--json] [--profile PATH] [--batch ID,...] [--use-case <swap|origin>]",
    );
    line(
        stderr,
        "  ramshared diagnose --events PATH [--json] | export --output FILE",
    );
    line(stderr, "  ramshared setup wizard [--json]");
    line(
        stderr,
        "  ramshared setup plan --profile NAME --output FILE [--scope storage-only|gpu-cache] [--json]",
    );
    line(
        stderr,
        "  ramshared setup apply --plan FILE --approval FILE [--json] (native storage only)",
    );
    line(
        stderr,
        "  ramshared setup reconcile --plan FILE --approval FILE [--json] (verify published native origin; remain Off)",
    );
    line(
        stderr,
        "  ramshared service enable-boot --approval FILE [--json] | disable-boot [--json] | status [--json]",
    );
    line(
        stderr,
        "  ramshared install --plan [--bundle-root DIR] [--lower-sink DIR] [--json]",
    );
    line(
        stderr,
        "  ramshared install --approve-nbd-product-install VER --approve-manifest-sha256 SHA256 \\",
    );
    line(
        stderr,
        "                    --lower-sink DIR --posture <writeback|swap> [--enroll-default] [--no-enroll] [--no-activate] \\",
    );
    line(
        stderr,
        "                    [--bundle-root DIR] [--target-release VER] [--running VER] [--previous-bundle DIR] \\",
    );
    line(
        stderr,
        "                    [--keyring FILE] [--policy FILE] [--approve-legacy-unit-replacement SHA256] \\",
    );
    line(
        stderr,
        "                    [--approve-legacy-auxiliary-unit-replacement UNIT:SHA256] [--json]",
    );
    line(
        stderr,
        "  ramshared install --auto-update [--check | --status] [--policy FILE] [--json]",
    );
    line(
        stderr,
        "  ramshared install --reconcile-rollback | --reconcile-complete [--bundle-root DIR] [--keyring FILE] [--policy FILE] [--json]",
    );
    line(
        stderr,
        "  ramshared package check-quiescent --action <upgrade|remove> [--json]",
    );
    line(
        stderr,
        "  ramshared up [--vram MiB] [--zram MiB] [--zram-backing-dev PATH] [--daemon PATH] [--connections N] \\",
    );
    line(
        stderr,
        "               [--transport auto|nbd|ublk] [--swap-dev PATH] [--nbd PATH] \\",
    );
    line(stderr, "               [--force-no-safety-net]");
    line(
        stderr,
        "  ramshared migrate-cascade --from-legacy  # attended one-time sealed-origin handoff",
    );
    line(
        stderr,
        "  ramshared ensure-origin  # idempotent sealed-origin attach (host-gate prerequisite)",
    );
    line(
        stderr,
        "  ramshared ensure-guardian  # idempotent sealed-guardian start (host-gate prerequisite)",
    );
    line(
        stderr,
        "  ramshared boot   # native fail-closed bootstrap (systemd unit entrypoint)",
    );
    line(stderr, "      up defaults: vram = sealed origin capacity");
    line(
        stderr,
        "      zram sizing: /etc/ramshared/cascade.conf, then RAMSHARED_ZRAM_MIB, then 1024 MiB",
    );
    line(
        stderr,
        "      boot sizing: /etc/ramshared/cascade.conf, then those env vars, then 1024 MiB each",
    );
    line(stderr, "      --zram 0  skip zram (VRAM/NBD only)");
    line(
        stderr,
        "      --zram-backing-dev PATH  bind the zram tier to a lower block device",
    );
    line(
        stderr,
        "      --zram-writeback-tier    make the VRAM tier zram's writeback target instead of a",
    );
    line(
        stderr,
        "                              swap device (the compressed spill lands on the tier; the",
    );
    line(
        stderr,
        "                              overflow reaches the fallback). Off by default.",
    );
    line(
        stderr,
        "      --accept-wsl2-nbd       arm NBD-backed swap on WSL2 (a peer failure can SIGBUS pages on the tier)",
    );
    line(
        stderr,
        "      --origin / --origin-manifest are refused: origin identity is sealed by the host gate",
    );
    line(
        stderr,
        "  ramshared status [--json]   # phase Armed/UsingVram/… + tiers",
    );
    line(
        stderr,
        "  ramshared top               # interactive real-time dashboard (TUI monitor)",
    );
    line(
        stderr,
        "  ramshared monitor [--jsonl] [--compact] [--once] [--interval-ms N] [--history-seconds N] [--lang en|pt-BR] \\",
    );
    line(
        stderr,
        "                    [--output PATH] [--heartbeat PATH] [--benchmark-evidence PATH]",
    );
    line(
        stderr,
        "      --output / --heartbeat require --jsonl; --compact is a plain-text variant",
    );
    line(
        stderr,
        "      --benchmark-evidence names the qualification record; the default is the installed release's",
    );
    line(
        stderr,
        "  ramshared stress|bench [--cascade] [--battery] [--full-three-tier] [--tier3-only] \\",
    );
    line(
        stderr,
        "                         [--start %] [--target %] [--step %] [--interval-ms N] [--hold-sec N] [--duration-sec N] \\",
    );
    line(
        stderr,
        "                         [--min-ram-mb N] [--threads N] \\",
    );
    line(
        stderr,
        "                         [--tier1-target-pct %] [--tier2-target-pct %] [--tier3-target-pct %] \\",
    );
    line(
        stderr,
        "                         [--physical-cache-target-mib N] [--max-psi-full %] [--max-latency-ms N] \\",
    );
    line(
        stderr,
        "                         [--min-order-7-chunks N] [--log PATH] [--json]",
    );
    line(
        stderr,
        "  ramshared down   # always swapoff before stopping the daemon (anti hang)",
    );
    line(
        stderr,
        "  ramshared down --plan [--json]   # read-only managed-swapoff headroom plan",
    );
    line(
        stderr,
        "  ramshared down --recover-released-peer   # bounded recovery of a lifecycle whose peer died",
    );
    line(
        stderr,
        "  ramshared swap-rebalance --plan --target <PATH|UUID=UUID> [--json]",
    );
    line(
        stderr,
        "  ramshared swap-rebalance --apply --target <PATH|UUID=UUID>",
    );
    line(
        stderr,
        "      one exact active fallback block device; --force, --all, --yes and -a are refused",
    );
    line(
        stderr,
        "      (attended apply; the read-only plan never changes swap state)",
    );
    line(
        stderr,
        "  ramshared release-sync   # move active product units onto the selected release",
    );
    line(stderr, "");
    line(stderr, "boot on WSL2 (opt-in, fail-closed):");
    // AUD-37: print only invocations the named script accepts. The no-argument
    // path is a read-only plan; applying a unit needs an attended version-scoped
    // approval. There is no `--enable`.
    line(
        stderr,
        "  sudo bash scripts/safety/install-cascade-boot.sh --plan   # read-only plan",
    );
    line(
        stderr,
        "  sudo bash scripts/safety/install-cascade-boot.sh \\",
    );
    line(stderr, "      --approve-nbd-product-install <version> \\");
    line(
        stderr,
        "      --approve-manifest-sha256 <digest> --lower-sink <dir> \\",
    );
    line(
        stderr,
        "      --posture <writeback|swap>   # attended, no default",
    );
    line(
        stderr,
        "  sudo bash scripts/safety/uninstall-cascade-boot.sh",
    );
}

/// Every bare `--flag` name the advertised surface knows about.
#[cfg(test)]
fn advertised_flags() -> Vec<&'static str> {
    let mut names: Vec<&'static str> = TOP_LEVEL_FLAGS.to_vec();
    for surface in SURFACES {
        names.extend_from_slice(surface.accepts);
        names.extend_from_slice(surface.refuses);
    }
    names.sort_unstable();
    names.dedup();
    names
}

/// Bare `--flag` tokens in `text`, in order of appearance.
///
/// A token is a run of `--` + lowercase ASCII / digits / hyphens that sits
/// between double quotes or immediately after whitespace, so a longer string
/// such as `"--events requires a path"` contributes nothing and a prose mention
/// in the help text is still counted.
#[cfg(test)]
fn flag_tokens(text: &str) -> Vec<&str> {
    let bytes = text.as_bytes();
    let mut found = Vec::new();
    let mut index = 0;
    while index + 2 < bytes.len() {
        if bytes[index] != b'-' || bytes[index + 1] != b'-' {
            index += 1;
            continue;
        }
        let start = index;
        let mut end = index + 2;
        while end < bytes.len()
            && (bytes[end].is_ascii_lowercase()
                || bytes[end].is_ascii_digit()
                || bytes[end] == b'-')
        {
            end += 1;
        }
        if end == index + 2 {
            index += 1;
            continue;
        }
        // Accept only whole tokens. A quoted token must be quoted on both
        // sides (`"--jsonl"`), so a longer string such as `"--events requires
        // a path"` contributes nothing. An unquoted token must be delimited
        // by whitespace or punctuation on both sides.
        let quoted = start > 0 && (bytes[start - 1] == b'"' || bytes[start - 1] == b'\'');
        let before_ok = start == 0 || quoted || {
            let prev = bytes[start - 1];
            prev.is_ascii_whitespace() || prev == b'='
        };
        let after_ok = if quoted {
            end < bytes.len() && (bytes[end] == b'"' || bytes[end] == b'\'')
        } else {
            end == bytes.len()
                || matches!(
                    bytes[end],
                    b'"' | b'\'' | b' ' | b'\t' | b'\n' | b'\r' | b',' | b')' | b']' | b'\\'
                )
        };
        if before_ok && after_ok {
            found.push(&text[start..end]);
            index = end;
        } else {
            index += 1;
        }
    }
    found
}

#[cfg(test)]
mod tests;
