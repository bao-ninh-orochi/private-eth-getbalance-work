//! `xtask` binary: repo-local developer tooling.
//!
//! ```text
//! xtask conformance [--blocks <u64>] [--addresses <usize>] [--seed <u64>] [--lwe-dim <u32>]
//! xtask bench [--write] [--scales <n,n,...>] [--mid-scale <u64>]
//! xtask web
//! xtask geometry [--accounts <u64>] [--arity <list>] [--bucket-size <list>] [--fingerprint-bits <u32>]
//!                 [--fill-check] [--fill-accounts <u64>]
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
//!
//! `geometry` sweeps `risepir_proto::geometry::Geometry` across `arity x
//! bucket_size` (ADR-0030) — see `xtask::geometry`'s module docs for why
//! this question keeps coming up. `--fill-check` additionally builds real
//! `segmented_cuckoo` stores and inserts into them for a small candidate
//! set (slow; opt-in only, never part of `cargo test`). `<list>` accepts
//! comma-separated values and/or inclusive ranges, e.g. `2,3,4` or `1-16`.

use xtask::conformance::{self, ConformanceConfig};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("conformance") => run_conformance(&args[2..]),
        Some("bench") => run_bench(&args[2..]),
        Some("geometry") => run_geometry(&args[2..]),
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
            "--blocks" => cfg.blocks = parse_value("xtask conformance", rest, &mut i, "--blocks"),
            "--addresses" => {
                cfg.min_addresses = parse_value("xtask conformance", rest, &mut i, "--addresses")
            }
            "--seed" => cfg.seed = parse_value("xtask conformance", rest, &mut i, "--seed"),
            "--lwe-dim" => {
                cfg.lwe_dim = parse_value("xtask conformance", rest, &mut i, "--lwe-dim")
            }
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
/// flags). `cmd` is only used to name the caller in an error message
/// (e.g. `"xtask conformance"`, `"xtask geometry"`).
fn parse_value<T: std::str::FromStr>(cmd: &str, args: &[String], i: &mut usize, name: &str) -> T {
    let Some(raw) = args.get(*i + 1) else {
        eprintln!("{cmd}: {name} requires a value");
        std::process::exit(2);
    };
    let value = raw.parse().unwrap_or_else(|_| {
        eprintln!("{cmd}: {name} got an invalid value: {raw:?}");
        std::process::exit(2);
    });
    *i += 2;
    value
}

/// Parses the value following flag `name` as a comma-separated list of
/// `u32`s, where each comma-separated token is either a bare integer
/// (`4`) or an inclusive range (`1-16`, expanded in full) — so
/// `--bucket-size 1-16` and `--bucket-size 1,2,3,...,16` mean the same
/// thing. Advances `*i` past both the flag and its value, exactly like
/// [`parse_value`].
fn parse_u32_list(cmd: &str, args: &[String], i: &mut usize, name: &str) -> Vec<u32> {
    let Some(raw) = args.get(*i + 1) else {
        eprintln!("{cmd}: {name} requires a value");
        std::process::exit(2);
    };
    let mut out = Vec::new();
    for token in raw.split(',') {
        match token.split_once('-') {
            Some((lo, hi)) => match (lo.parse::<u32>(), hi.parse::<u32>()) {
                (Ok(lo), Ok(hi)) if lo <= hi => out.extend(lo..=hi),
                _ => {
                    eprintln!(
                        "{cmd}: {name}: invalid range {token:?} (expected LO-HI with LO <= HI)"
                    );
                    std::process::exit(2);
                }
            },
            None => match token.parse::<u32>() {
                Ok(v) => out.push(v),
                Err(_) => {
                    eprintln!("{cmd}: {name}: invalid value {token:?}");
                    std::process::exit(2);
                }
            },
        }
    }
    *i += 2;
    out
}

/// Parses the comma-separated `u64` list following `--scales`, advancing
/// `*i` past both the flag and its value. Rejects an empty list rather than
/// letting it reach `bench::run`'s own assertion, so the error names the
/// flag the caller actually typed.
fn parse_scale_list(args: &[String], i: &mut usize) -> Vec<u64> {
    let Some(raw) = args.get(*i + 1) else {
        eprintln!("xtask bench: --scales requires a comma-separated list of account counts");
        std::process::exit(2);
    };
    let scales: Vec<u64> = raw
        .split(',')
        .map(|s| {
            s.trim().parse::<u64>().unwrap_or_else(|_| {
                eprintln!("xtask bench: --scales got an invalid account count: {s:?}");
                std::process::exit(2);
            })
        })
        .collect();
    if scales.is_empty() || scales.contains(&0) {
        eprintln!("xtask bench: --scales must list at least one nonzero account count");
        std::process::exit(2);
    }
    *i += 2;
    scales
}

fn print_usage() {
    eprintln!("usage: xtask conformance [--blocks <u64>] [--addresses <usize>] [--seed <u64>] [--lwe-dim <u32>]");
    eprintln!("       xtask bench [--write] [--scales <n,n,...>] [--mid-scale <u64>]");
    eprintln!(
        "       xtask web                 (build the browser client's wasm into web/client.wasm)"
    );
    eprintln!(
        "       xtask geometry [--accounts <u64>] [--arity <list>] [--bucket-size <list>] \
         [--fingerprint-bits <u32>] [--fill-check] [--fill-accounts <u64>]"
    );
}

/// Runs the Stage 3 measured numbers table
/// (`xtask::bench::BenchConfig::default()`), prints it to stdout, and — only
/// with `--write` — overwrites `docs/numbers.md` (see the "IKPIR build" note
/// the report carries for why the write is opt-in). The harness is
/// deterministic (fixed mock seed) by design so re-runs are comparable, per
/// the brief.
///
/// `--scales` overrides the account scales the sweep runs at. It exists for
/// one specific question: the headline rebuild ÷ patch ratio (§6) is
/// published only up to 9,437,184 accounts, and extending the *trend* to
/// larger scales is what makes an extrapolation to the deployed 200 M set
/// defensible rather than invented. The top scale still passes through the
/// harness's own RAM safety fallback, so asking for more than this machine
/// can hold degrades to the largest scale that fits and says so, rather
/// than swapping (which would silently corrupt every timing in the run).
fn run_bench(rest: &[String]) {
    let mut write = false;
    let mut scales: Option<Vec<u64>> = None;
    let mut mid_scale: Option<u64> = None;
    let mut i = 0;
    while i < rest.len() {
        match rest[i].as_str() {
            "--write" => {
                write = true;
                i += 1;
            }
            "--scales" => scales = Some(parse_scale_list(rest, &mut i)),
            "--mid-scale" => {
                mid_scale = Some(parse_value("xtask bench", rest, &mut i, "--mid-scale"))
            }
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

    let mut cfg = xtask::bench::BenchConfig::default();
    if let Some(scales) = scales {
        cfg.scales = scales;
        // The patch curve / delta-bytes / answer-latency sections run at
        // `mid_scale`, which `bench::run` asserts is one of `scales`. An
        // explicit `--mid-scale` wins; otherwise keep the default if the
        // caller's list still contains it, and fall back to the smallest
        // requested scale if it does not — never leave the two
        // inconsistent, which would abort the run inside the harness.
        cfg.mid_scale = mid_scale.unwrap_or_else(|| {
            if cfg.scales.contains(&cfg.mid_scale) {
                cfg.mid_scale
            } else {
                *cfg.scales
                    .iter()
                    .min()
                    .expect("--scales rejects an empty list")
            }
        });
    } else if let Some(mid) = mid_scale {
        cfg.mid_scale = mid;
    }
    if !cfg.scales.contains(&cfg.mid_scale) {
        eprintln!(
            "xtask bench: --mid-scale {} is not one of --scales {:?}",
            cfg.mid_scale, cfg.scales
        );
        std::process::exit(2);
    }
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
        let out_path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/numbers.md");
        std::fs::write(&out_path, &markdown)
            .unwrap_or_else(|e| panic!("xtask bench: failed to write {}: {e}", out_path.display()));
        println!(
            "Wrote {} — ensure this build was against the pinned IKPIR rev (root Cargo.toml).",
            out_path.display()
        );
    } else {
        println!("(printed only; pass `--write` to overwrite docs/numbers.md — only from a build against the pinned IKPIR rev)");
    }
}

/// Runs the `xtask::geometry` arithmetic sweep (`xtask::geometry::SweepConfig::default()`
/// unless overridden) and prints it; with `--fill-check`, additionally
/// runs the slow, real-store fill-check (`xtask::geometry::DEFAULT_FILL_CANDIDATES`)
/// and prints that too. See `xtask::geometry`'s module docs (ADR-0030) for
/// why this question keeps coming up and what each half of this command
/// actually measures vs. computes.
fn run_geometry(rest: &[String]) {
    const CMD: &str = "xtask geometry";
    let mut cfg = xtask::geometry::SweepConfig::default();
    let mut do_fill_check = false;
    let mut fill_accounts = xtask::geometry::DEFAULT_FILL_ACCOUNTS;

    let mut i = 0;
    while i < rest.len() {
        match rest[i].as_str() {
            "--accounts" => cfg.accounts = parse_value(CMD, rest, &mut i, "--accounts"),
            "--arity" => cfg.arities = parse_u32_list(CMD, rest, &mut i, "--arity"),
            "--bucket-size" => {
                cfg.bucket_sizes = parse_u32_list(CMD, rest, &mut i, "--bucket-size")
            }
            "--fingerprint-bits" => {
                cfg.fingerprint_bits = parse_value(CMD, rest, &mut i, "--fingerprint-bits")
            }
            "--fill-accounts" => fill_accounts = parse_value(CMD, rest, &mut i, "--fill-accounts"),
            "--fill-check" => {
                do_fill_check = true;
                i += 1;
            }
            "--help" | "-h" => {
                print_usage();
                std::process::exit(0);
            }
            other => {
                eprintln!("{CMD}: unknown argument: {other}");
                print_usage();
                std::process::exit(2);
            }
        }
    }

    println!(
        "Running geometry sweep: {} accounts, arity {:?}, bucket_size {:?}..={:?}, fingerprint_bits {} \
         (docs/deploy.md §5.3, ADR-0030)",
        cfg.accounts,
        cfg.arities,
        cfg.bucket_sizes.first(),
        cfg.bucket_sizes.last(),
        cfg.fingerprint_bits,
    );
    let accounts = cfg.accounts;
    match xtask::geometry::sweep(&cfg) {
        Ok(rows) => print!("{}", xtask::geometry::render_sweep_table(&rows, accounts)),
        Err(e) => {
            eprintln!("{CMD}: {e}");
            std::process::exit(1);
        }
    }

    if do_fill_check {
        println!();
        println!(
            "Running fill-check: {} accounts x {} candidates — real segmented_cuckoo::CuckooKVStore, real \
             inserts (this is slow; the machine may be busy with other work).",
            fill_accounts,
            xtask::geometry::DEFAULT_FILL_CANDIDATES.len()
        );
        let results = xtask::geometry::fill_check(
            &xtask::geometry::DEFAULT_FILL_CANDIDATES,
            fill_accounts,
            cfg.fingerprint_bits,
        );
        print!("{}", xtask::geometry::render_fill_check(&results));
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
    let output = std::process::Command::new("sysctl")
        .args(["-n", name])
        .output()
        .ok()?;
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
