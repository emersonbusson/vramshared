// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/usage/tests.rs
#![allow(clippy::expect_used, clippy::unwrap_used)]

use super::*;

/// Source of one sub-parser, read at compile time so the reverse gate
/// cannot pass on a stale checkout.
const PARSER_SOURCES: &[(&str, &str)] = &[
    ("main.rs", include_str!("../main.rs")),
    // The top-level parser lives in `command.rs` after the extraction
    // slice; `main.rs` is still scanned because the dispatch and subprocess
    // argv surfaces remain there and the gate must not go blind on them.
    ("command.rs", include_str!("../command.rs")),
    ("monitor.rs", include_str!("../monitor.rs")),
    ("stress.rs", include_str!("../stress.rs")),
    ("cascade/mod.rs", include_str!("../cascade/mod.rs")),
    ("diagnose.rs", include_str!("../diagnose.rs")),
    ("workload.rs", include_str!("../workload.rs")),
    ("setup.rs", include_str!("../setup.rs")),
    (
        "customer/setup_apply.rs",
        include_str!("../customer/setup_apply.rs"),
    ),
    (
        "customer/install_args.rs",
        include_str!("../customer/install_args.rs"),
    ),
    (
        "customer/package_hook.rs",
        include_str!("../customer/package_hook.rs"),
    ),
    ("service.rs", include_str!("../service.rs")),
];

/// The production half of a parser source: everything before the first
/// top-level `mod tests`. Test fixtures invent argv shapes and must not
/// widen the advertised surface.
fn production_half(source: &str) -> &str {
    match source.find("\nmod tests") {
        Some(index) => &source[..index],
        None => match source.find("\n#[cfg(test)]") {
            Some(index) => &source[..index],
            None => source,
        },
    }
}

/// A `--flag` the parser reads: a standalone quoted token that is either
/// a match-arm pattern (`"--flag" =>`, `"--flag" |`, `"--flag" if`) or a
/// compared value (`== "--flag"`). Longer error strings and subprocess
/// argv (`"--scope".into()`) contribute nothing.
fn parser_flags(source: &str) -> Vec<&str> {
    let production = production_half(source);
    let mut found = Vec::new();
    let bytes = production.as_bytes();
    let mut index = 0;
    while index + 2 < bytes.len() {
        if bytes[index] != b'"' {
            index += 1;
            continue;
        }
        let open = index;
        let mut close = open + 1;
        while close < bytes.len() && bytes[close] != b'"' {
            close += 1;
        }
        if close >= bytes.len() {
            break;
        }
        let token = &production[open + 1..close];
        let is_flag = token.starts_with("--")
            && token.len() > 2
            && token[2..]
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
        if is_flag {
            let mut look = close + 1;
            while look < bytes.len() && bytes[look].is_ascii_whitespace() {
                look += 1;
            }
            let arm = look < bytes.len()
                && (bytes[look] == b'=' && look + 1 < bytes.len() && bytes[look + 1] == b'>'
                    || bytes[look] == b'|'
                    || production[look..].starts_with("if"));
            let compared = production[..open].ends_with("== ");
            if arm || compared {
                found.push(token);
            }
        }
        index = close + 1;
    }
    found.sort_unstable();
    found.dedup();
    found
}

fn rendered_usage() -> String {
    let mut buf = Vec::new();
    write_usage(&mut buf);
    String::from_utf8(buf).expect("usage text is UTF-8")
}

#[test]
fn usage_text_names_every_advertised_flag() {
    let usage = rendered_usage();
    for flag in advertised_flags() {
        assert!(
            usage.contains(flag),
            "usage text must name the advertised flag {flag}"
        );
    }
}

#[test]
fn usage_never_advertises_an_unparsed_flag() {
    // The help text is what an operator types. A `--flag` it names that no
    // parser reads is the OPERATOR-GUIDE defect class: documented, and
    // refused at the prompt. The `boot on WSL2` block is a different
    // program (`install-cascade-boot.sh`) and is out of this contract.
    let usage = rendered_usage();
    let ramshared_section = match usage.find("\nboot on WSL2") {
        Some(index) => &usage[..index],
        None => usage.as_str(),
    };
    let advertised = advertised_flags();
    for flag in flag_tokens(ramshared_section) {
        assert!(
            advertised.contains(&flag),
            "usage text advertises {flag}, which no parser reads"
        );
    }
}

#[test]
fn parser_match_arms_are_advertised() {
    let advertised = advertised_flags();
    for (name, source) in PARSER_SOURCES {
        for flag in parser_flags(source) {
            assert!(
                advertised.contains(&flag),
                "{name} parses {flag}, which the advertised surface does not name"
            );
        }
    }
}

#[test]
fn every_accepted_flag_is_accepted_by_its_parser() {
    // Each catalogued flag must survive the parser that owns it. Shapes
    // below are the smallest argv that exercises the flag without
    // reaching an effect: parse-level validation only.
    let monitor = crate::monitor::MonitorOptions::parse(&["--jsonl".into(), "--compact".into()]);
    assert!(monitor.is_ok(), "--jsonl/--compact must parse");
    assert!(crate::monitor::MonitorOptions::parse(&["--once".into()]).is_ok());
    assert!(
        crate::monitor::MonitorOptions::parse(&[
            "--jsonl".into(),
            "--interval-ms".into(),
            "1000".into(),
            "--history-seconds".into(),
            "30".into(),
            "--output".into(),
            "/tmp/m.jsonl".into(),
            "--heartbeat".into(),
            "/tmp/h.json".into()
        ])
        .is_ok()
    );

    let stress = strings(&[
        "--start",
        "10",
        "--target",
        "20",
        "--step",
        "5",
        "--interval-ms",
        "500",
        "--hold-sec",
        "10",
        "--duration-sec",
        "180",
        "--min-ram-mb",
        "256",
        "--threads",
        "1",
        "--battery",
        "--cascade",
        "--tier1-target-pct",
        "95",
        "--tier2-target-pct",
        "95",
        "--tier3-target-pct",
        "45",
        "--physical-cache-target-mib",
        "256",
        "--max-psi-full",
        "20",
        "--max-latency-ms",
        "100",
        "--min-order-7-chunks",
        "0",
        "--log",
        "/tmp/s.jsonl",
        "--json",
    ]);
    assert!(
        crate::stress::parse_stress_args(&stress).is_ok(),
        "every advertised stress flag must parse"
    );
    assert!(crate::stress::parse_stress_args(&strings(&["--full-three-tier"])).is_ok());
    assert!(
        crate::stress::parse_stress_args(&strings(&["--tier3-only", "--tier3-target-pct", "45"]))
            .is_ok()
    );

    // `--origin` / `--origin-manifest` are catalogued as refuses: the
    // parser must name them, and must not honour them.
    for sealed_flag in ["--origin", "--origin-manifest"] {
        let sealed = crate::cascade::parse_up_args_verdict_for_test(&[sealed_flag, "/tmp/x"]);
        assert!(
            sealed.is_err(),
            "{sealed_flag} must be refused as sealed, not accepted"
        );
    }
    // 4096 is the sealed logical capacity of the `cfg(test)` origin
    // fixture; any other `--vram` is refused before the flags below are
    // exercised, which would make this a refusal test in disguise.
    let up = crate::cascade::parse_up_args_verdict_for_test(&[
        "--vram",
        "4096",
        "--zram",
        "1024",
        "--daemon",
        "ramsharedd",
        "--connections",
        "1",
        "--transport",
        "nbd",
        "--swap-dev",
        "/dev/ramshared0",
        "--nbd",
        "/dev/ramshared0",
        "--force-no-safety-net",
    ]);
    assert!(up.is_ok(), "every advertised up flag must parse");

    for bench_args in [
        vec![
            "config",
            "benchmark",
            "--plan",
            "--json",
            "--profile",
            "/tmp/p.toml",
            "--use-case",
            "swap",
        ],
        vec![
            "config",
            "benchmark",
            "--confirm",
            "--batch",
            "fs-1,fs-2",
            "--use-case",
            "origin",
        ],
        vec!["config", "benchmark", "--recover", "--json"],
    ] {
        assert!(
            crate::command::parse_cli_command(&strings(&bench_args)).is_ok(),
            "{bench_args:?} must parse"
        );
    }

    for install_case in [
        vec![
            "--plan",
            "--bundle-root",
            "/tmp/b",
            "--lower-sink",
            "/tmp/s",
            "--json",
        ],
        vec![
            "--approve-nbd-product-install",
            "v1",
            "--approve-manifest-sha256",
            &"a".repeat(64),
            "--lower-sink",
            "/tmp/s",
            "--posture",
            "writeback",
            "--enroll-default",
            "--target-release",
            "v1",
            "--running",
            "v0",
            "--previous-bundle",
            "/tmp/p",
            "--keyring",
            "/tmp/k",
            "--approve-legacy-unit-replacement",
            &"b".repeat(64),
            "--approve-legacy-auxiliary-unit-replacement",
            &format!("ramshared-cascade-health.service:{}", "c".repeat(64)),
        ],
        vec![
            "--auto-update",
            "--check",
            "--policy",
            "/tmp/p",
            "--no-enroll",
            "--no-activate",
        ],
        vec!["--status", "--reconcile-rollback"],
        vec!["--reconcile-complete"],
    ] {
        assert!(
            crate::command::parse_cli_command(&{
                let mut v = vec!["install".to_string()];
                v.extend(install_case.iter().map(|s| (*s).to_string()));
                v
            })
            .is_ok()
        );
    }
    assert!(
        crate::command::parse_cli_command(&strings(&[
            "package",
            "check-quiescent",
            "--action",
            "upgrade",
            "--json"
        ]))
        .is_ok()
    );
}

#[test]
fn usage_help_names_swap_rebalance_refusals() {
    let usage = rendered_usage();
    assert!(usage.contains("ramshared swap-rebalance --plan --target <PATH|UUID=UUID> [--json]"));
    assert!(usage.contains("ramshared swap-rebalance --apply --target <PATH|UUID=UUID>"));
    assert!(usage.contains("--force, --all, --yes and -a are refused"));

    let surface = SURFACES
        .iter()
        .find(|surface| surface.command == "swap-rebalance")
        .expect("the catalog advertises swap-rebalance");
    assert_eq!(
        surface.accepts,
        &["--plan", "--apply", "--target", "--json"]
    );
    assert_eq!(surface.refuses, &["--force", "--all", "--yes"]);

    for args in [
        vec![
            "swap-rebalance",
            "--plan",
            "--target",
            "/dev/sdf",
            "--force",
        ],
        vec!["swap-rebalance", "--plan", "--target", "/dev/sdf", "--all"],
        vec!["swap-rebalance", "--plan", "--target", "/dev/sdf", "--yes"],
        vec!["swap-rebalance", "--plan", "--target", "/dev/sdf", "-a"],
    ] {
        assert!(
            crate::command::parse_cli_command(&strings(&args)).is_err(),
            "{args:?} must be refused"
        );
    }
}

fn strings(args: &[&str]) -> Vec<String> {
    args.iter().map(|arg| (*arg).to_string()).collect()
}

#[test]
fn operator_guide_examples_use_only_advertised_flags() {
    // Operator-facing docs sit outside the crate, so they are bound here
    // at compile time: an example line that names a flag no parser reads
    // is the same defect as a wrong help text.
    let guides = [
        (
            "operations.rst",
            include_str!("../../../../Documentation/admin-guide/operations.rst"),
        ),
        (
            "configuration.rst",
            include_str!("../../../../Documentation/admin-guide/configuration.rst"),
        ),
    ];
    let advertised = advertised_flags();
    let mut checked = 0;
    for (name, guide) in guides {
        for raw in guide.lines() {
            let line = raw.trim();
            let example = match line.strip_prefix("$ ") {
                Some(rest) => rest.trim_start(),
                None => line,
            };
            // Only single-command examples of this binary. Installer scripts
            // (`sudo bash scripts/.../install-cascade-boot.sh --plan`) are a
            // different program with their own flags.
            if !example.starts_with("ramshared ") {
                continue;
            }
            if example.contains(" | ") || example.contains(" and ") {
                continue;
            }
            for flag in flag_tokens(example) {
                assert!(
                    advertised.contains(&flag),
                    "{name} example advertises {flag}, which no parser reads: {line}"
                );
            }
            checked += 1;
        }
    }
    assert!(
        checked >= 3,
        "operator docs must keep their runnable examples; found {checked}"
    );
}

#[test]
fn surface_catalog_is_well_formed() {
    for surface in SURFACES {
        assert!(
            !surface.command.is_empty(),
            "every surface needs a command word"
        );
        for flag in surface.accepts.iter().chain(surface.refuses.iter()) {
            assert!(
                flag.starts_with("--") && flag.len() > 2,
                "catalog flag must be a bare --name: {flag}"
            );
            assert!(
                !surface.accepts.contains(flag)
                    || surface.accepts.iter().filter(|f| **f == *flag).count() == 1,
                "flag {flag} is catalogued twice on {}",
                surface.command
            );
        }
        for flag in surface.refuses {
            assert!(
                !surface.accepts.contains(flag),
                "flag {flag} is both accepted and refused on {}",
                surface.command
            );
        }
    }
    // Shared spellings are real (`--json` is the report flag on several
    // verbs), so the catalog allows a flag on more than one surface. What
    // it must never do is leave a verb's own flag off that verb.
    for flag in TOP_LEVEL_FLAGS {
        assert!(
            advertised_flags().contains(flag),
            "{flag} must stay catalogued"
        );
    }
}

#[test]
fn flag_tokens_ignore_longer_strings() {
    let tokens = flag_tokens(r#"use --events PATH or "--events requires a path" and --jsonl"#);
    assert!(tokens.contains(&"--events"));
    assert!(tokens.contains(&"--jsonl"));
    // The prose string is not a standalone token.
    assert_eq!(
        tokens.iter().filter(|token| **token == "--events").count(),
        1
    );
}

#[test]
fn parser_flags_skip_subprocess_argv() {
    // `systemctl --scope` / `swapon --show` are not ramshared CLI flags.
    let source = r#"
fn f() {
let a = ["--scope".into()];
let b = sh("swapon", &["--show"]);
match x {
    "--jsonl" => (),
    "--real" if y => (),
    _ => (),
}
if option == "--compared" { }
}
mod tests { }
"#;
    let flags = parser_flags(source);
    assert!(flags.contains(&"--jsonl"));
    assert!(flags.contains(&"--real"));
    assert!(flags.contains(&"--compared"));
    assert!(!flags.contains(&"--scope"));
    assert!(!flags.contains(&"--show"));
}
