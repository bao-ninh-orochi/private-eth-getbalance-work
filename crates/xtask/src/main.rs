//! `xtask` binary: repo-local developer tooling.
//!
//! ```text
//! xtask conformance [--blocks <u64>] [--addresses <usize>] [--seed <u64>] [--lwe-dim <u32>]
//! ```
//!
//! Runs the Stage 0.5 conformance harness (`docs/plan.md` §6, §8) and
//! exits `0` on pass, `1` on fail — the one pass/fail command Stage 0.5
//! asks for. Omitted flags default to the real gate
//! (`xtask::conformance::ConformanceConfig::default()`).

use xtask::conformance::{self, ConformanceConfig};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("conformance") => run_conformance(&args[2..]),
        Some("--help" | "-h") | None => {
            print_usage();
            std::process::exit(0);
        }
        Some(other) => {
            eprintln!("xtask: unknown subcommand: {other}");
            print_usage();
            std::process::exit(2);
        }
    }
}

fn run_conformance(rest: &[String]) {
    let mut cfg = ConformanceConfig::default();
    let mut i = 0;
    while i < rest.len() {
        match rest[i].as_str() {
            "--blocks" => cfg.blocks = parse_value(rest, &mut i, "--blocks"),
            "--addresses" => cfg.min_addresses = parse_value(rest, &mut i, "--addresses"),
            "--seed" => cfg.seed = parse_value(rest, &mut i, "--seed"),
            "--lwe-dim" => cfg.lwe_dim = parse_value(rest, &mut i, "--lwe-dim"),
            "--help" | "-h" => {
                print_usage();
                std::process::exit(0);
            }
            other => {
                eprintln!("xtask conformance: unknown argument: {other}");
                print_usage();
                std::process::exit(2);
            }
        }
    }

    println!(
        "Running conformance: {} genesis accounts, {} blocks, target >= {} addresses, {} checkpoints, lwe_dim={}",
        cfg.num_genesis_keys, cfg.blocks, cfg.min_addresses, cfg.checkpoints, cfg.lwe_dim
    );

    let report = conformance::run(&cfg);
    println!("{report}");
    println!();

    if report.passed {
        println!(
            "CONFORMANCE: PASS ({} addresses x {} blocks, {} checks, 0 mismatches)",
            report.sample_size, report.blocks, report.total_checks
        );
        std::process::exit(0);
    } else {
        println!(
            "CONFORMANCE: FAIL ({} addresses x {} blocks, {} checks, {} mismatches)",
            report.sample_size,
            report.blocks,
            report.total_checks,
            report.mismatches.len()
        );
        for m in report.mismatches.iter().take(5) {
            println!("  {m}");
        }
        std::process::exit(1);
    }
}

/// Parses the value following flag `name`, advancing `*i` past both the
/// flag and its value — mirrors `risepir-rpc`'s own hand-rolled flag
/// parsing (no `clap` dependency for a handful of optional numeric
/// flags).
fn parse_value<T: std::str::FromStr>(args: &[String], i: &mut usize, name: &str) -> T {
    let Some(raw) = args.get(*i + 1) else {
        eprintln!("xtask conformance: {name} requires a value");
        std::process::exit(2);
    };
    let value = raw.parse().unwrap_or_else(|_| {
        eprintln!("xtask conformance: {name} got an invalid value: {raw:?}");
        std::process::exit(2);
    });
    *i += 2;
    value
}

fn print_usage() {
    eprintln!("usage: xtask conformance [--blocks <u64>] [--addresses <usize>] [--seed <u64>] [--lwe-dim <u32>]");
}
