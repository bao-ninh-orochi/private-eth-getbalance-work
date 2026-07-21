//! `xtask` binary: repo-local developer tooling.
//!
//! ```text
//! xtask conformance [--blocks <u64>] [--addresses <usize>] [--seed <u64>] [--lwe-dim <u32>]
//! xtask bench [--write]
//! xtask web
//! ```
//!
//! `conformance` runs the Stage 0.5 conformance harness (`docs/plan.md`
//! §6, §8) and exits `0` on pass, `1` on fail — the one pass/fail command
//! Stage 0.5 asks for. Omitted flags default to the real gate
//! (`xtask::conformance::ConformanceConfig::default()`).
//!
//! `bench` runs the Stage 3 measured numbers table (`docs/plan.md` §7,
//! `docs/verification.md` §7) — see `xtask::bench` — and prints it to
//! stdout; pass `--write` to also overwrite `docs/numbers.md` (a curated
//! reference measured against the pinned IKPIR perf/optimized rev — only overwrite
//! from that build). Always run with `--release` (see `xtask::bench`'s module
//! docs for why).

use xtask::conformance::{self, ConformanceConfig};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("conformance") => run_conformance(&args[2..]),
        Some("bench") => run_bench(&args[2..]),
        Some("web") => {
            xtask::web::run();
        }
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
    eprintln!("       xtask bench [--write]");
    eprintln!("       xtask web                 (build the browser client's wasm into web/client.wasm)");
}

/// Runs the Stage 3 measured numbers table
/// (`xtask::bench::BenchConfig::default()`), prints it to stdout, and — only
/// with `--write` — overwrites `docs/numbers.md` (see the "IKPIR build" note
/// the report carries for why the write is opt-in). The harness is
/// deterministic (fixed mock seed) by design so re-runs are comparable, per
/// the brief.
fn run_bench(rest: &[String]) {
    let mut write = false;
    for arg in rest {
        match arg.as_str() {
            "--write" => write = true,
            "--help" | "-h" => {
                print_usage();
                std::process::exit(0);
            }
            other => {
                eprintln!("xtask bench: unknown argument: {other}");
                print_usage();
                std::process::exit(2);
            }
        }
    }

    let cfg = xtask::bench::BenchConfig::default();
    println!(
        "Running bench: scales {:?}, mid_scale {}, K values {:?} (docs/plan.md §7, docs/verification.md §7)",
        cfg.scales, cfg.mid_scale, cfg.k_values
    );

    let report = xtask::bench::run(&cfg);
    let markdown = report.to_markdown(&machine_note(), &bench_date());

    println!();
    println!("{markdown}");

    // `docs/numbers.md` is a *curated reference* measured against IKPIR
    // the pinned perf/optimized rev — see the "IKPIR build" note the report itself
    // carries. Overwriting it is therefore an explicit, opt-in choice
    // (`--write`), and should only be done from a build against the pinned rev, so a
    // casual run against the current `main`-based local path-dep cannot silently
    // clobber the reference with slower single-threaded numbers.
    if write {
        let out_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/numbers.md");
        std::fs::write(&out_path, &markdown)
            .unwrap_or_else(|e| panic!("xtask bench: failed to write {}: {e}", out_path.display()));
        println!("Wrote {} — ensure this build was against the pinned IKPIR rev (root Cargo.toml).", out_path.display());
    } else {
        println!("(printed only; pass `--write` to overwrite docs/numbers.md — only from a build against the pinned IKPIR rev)");
    }
}

/// Best-effort machine description via `sysctl` (macOS core count / RAM),
/// falling back to the static label `docs/verification.md` itself uses if
/// `sysctl` is unavailable (e.g. a non-macOS host).
fn machine_note() -> String {
    let cores = sysctl_u64("hw.physicalcpu");
    let mem_bytes = sysctl_u64("hw.memsize");
    match (cores, mem_bytes) {
        (Some(cores), Some(mem)) => {
            let gib = mem as f64 / (1024.0 * 1024.0 * 1024.0);
            format!("{cores}-core Apple Silicon, {gib:.0} GB RAM, target-cpu=native (.cargo/config.toml)")
        }
        _ => "8-core Apple Silicon, 16 GB RAM, target-cpu=native, lwe_dim=1275, ADR-0009 144-bit value \
              (docs/verification.md's environment line)"
            .to_string(),
    }
}

fn sysctl_u64(name: &str) -> Option<u64> {
    let output = std::process::Command::new("sysctl").args(["-n", name]).output().ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()?.trim().parse().ok()
}

/// Real wall-clock date the bench actually ran, via the `date` command, so
/// `docs/numbers.md` is stamped automatically rather than hand-typed.
fn bench_date() -> String {
    std::process::Command::new("date")
        .arg("+%Y-%m-%d")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown date".to_string())
}
