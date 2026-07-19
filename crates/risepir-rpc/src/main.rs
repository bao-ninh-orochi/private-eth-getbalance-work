//! `risepir-rpc` binary — two deployments behind one JSON-RPC front end:
//!
//! ```text
//! risepir-rpc mock    [--chain-id <u64>] [--rpc-port <u16>] [--pir-port <u16>] [--proxy-upstream <url>]
//! risepir-rpc mainnet [--snapshot <csv[.gz]>]... [--snapshot-block <N>] [--snapshot-accounts <N>]
//!                     [--state <file>] [--partial] [--partial-capacity <N>]
//!                     [--feed-url <url>] [--confirm-url <url>]
//!                     [--rpc-port <u16>] [--pir-port <u16>] [--proxy-upstream <url>]
//!                     [--reconcile-every <blocks>] [--reconcile-samples <N>] [--lwe-dim <N>]
//! ```
//!
//! `mock` is the Stage-0 demo (synthetic chain, small LWE dim, seeded
//! demo accounts). `mainnet` is the real thing (`docs/deploy.md`):
//! snapshot/state/partial bootstrap, live `finalized` follow over public
//! RPC, cross-provider reconciliation, default LWE parameters.

use risepir_rpc::demo::{self, DemoConfig};
use risepir_rpc::mainnet::{self, MainnetConfig};

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("mock") => run_mock(parse_mock(&args[2..])).await,
        Some("mainnet") => run_mainnet(parse_mainnet(&args[2..])).await,
        Some("--help" | "-h") => {
            print_usage();
        }
        // Back-compat: the Stage-0.4 binary took mock's flags with no
        // subcommand. Keep that working, with a pointer to the new form.
        Some(flag) if flag.starts_with("--") => {
            eprintln!("risepir-rpc: note: no subcommand given; assuming `mock` (see --help)");
            run_mock(parse_mock(&args[1..])).await;
        }
        Some(other) => {
            eprintln!("risepir-rpc: unknown subcommand: {other}");
            print_usage();
            std::process::exit(2);
        }
        None => {
            eprintln!("risepir-rpc: note: no subcommand given; assuming `mock` (see --help)");
            run_mock(DemoConfig::default()).await;
        }
    }
}

async fn run_mock(cfg: DemoConfig) {
    if let Some(url) = &cfg.proxy_upstream {
        print_proxy_warning(url);
    }
    let handle = demo::spawn(cfg).await;

    println!("RisePIR private eth_getBalance — mock demo");
    println!("  PIR HTTP transport: http://{}", handle.pir_addr);
    println!("  JSON-RPC:           http://{}", handle.rpc_addr);
    println!();
    println!("Demo accounts (deterministic, wei-exact — query any of them):");
    for (addr, balance) in &handle.demo_accounts {
        println!("  0x{}  =>  {balance} wei", hex(addr));
    }
    println!();
    println!("Try:");
    if let Some((addr, _)) = handle.demo_accounts.first() {
        println!("  cast balance 0x{} --rpc-url http://{}", hex(addr), handle.rpc_addr);
    }
    println!("  cast chain-id --rpc-url http://{}", handle.rpc_addr);
    println!("  cast block-number --rpc-url http://{}", handle.rpc_addr);
    println!();

    // Every server runs as a detached tokio task; keep the runtime alive.
    std::future::pending::<()>().await;
}

async fn run_mainnet(cfg: MainnetConfig) {
    if let Some(url) = &cfg.proxy_upstream {
        print_proxy_warning(url);
    }
    let state_path = cfg.state.clone();
    let handle = mainnet::spawn(cfg).await;

    println!("RisePIR private eth_getBalance — Ethereum mainnet");
    println!("  PIR HTTP transport: http://{}", handle.pir_addr);
    println!("  JSON-RPC:           http://{}", handle.rpc_addr);
    println!(
        "  data set:           {}",
        if handle.complete {
            "COMPLETE nonzero-balance set (not-found answers 0x0)"
        } else {
            "PARTIAL (only accounts touched since bootstrap; not-found ERRORS, never 0x0)"
        }
    );
    println!("  serving from block: {} (follows finalized, ~13 min behind head)", handle.head_at_start);
    println!();
    println!("Try:");
    println!("  cast balance <address> --rpc-url http://{}", handle.rpc_addr);
    println!("  cast block-number --rpc-url http://{}", handle.rpc_addr);
    if state_path.is_some() {
        println!();
        println!("Ctrl-C saves state before exiting.");
    }

    tokio::signal::ctrl_c().await.expect("install Ctrl-C handler");
    if let Some(path) = state_path {
        eprintln!("risepir-rpc mainnet: Ctrl-C — saving state to {} ...", path.display());
        let codec = mainnet_value_codec();
        let complete = handle.complete;
        let result = handle
            .node
            .with_server(|server| risepir_rpc::state::save(server, &codec, complete, &path))
            .await;
        match result {
            Ok(()) => eprintln!("risepir-rpc mainnet: state saved; exiting"),
            Err(e) => eprintln!("risepir-rpc mainnet: WARNING: state save failed: {e}"),
        }
    }
    std::process::exit(0);
}

/// The mainnet deployment's fixed value codec — mirrors
/// `mainnet::value_codec` (crate-private there; the widths are part of
/// the deployment's identity, not a CLI knob).
fn mainnet_value_codec() -> risepir_proto::ValueCodec {
    risepir_proto::ValueCodec {
        key_tag_bits: 32,
        balance_bits: 96,
        checksum_bits: 16,
    }
}

// ─── flag parsing (hand-rolled; see the Stage-0.4 note in git history) ──

fn parse_mock(args: &[String]) -> DemoConfig {
    let mut cfg = DemoConfig::default();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--chain-id" => cfg.chain_id = parse_next(args, &mut i, "--chain-id"),
            "--rpc-port" => cfg.rpc_port = parse_next(args, &mut i, "--rpc-port"),
            "--pir-port" => cfg.pir_port = parse_next(args, &mut i, "--pir-port"),
            "--proxy-upstream" => cfg.proxy_upstream = Some(next_value(args, &mut i, "--proxy-upstream")),
            "--help" | "-h" => {
                print_usage();
                std::process::exit(0);
            }
            other => unknown(other),
        }
    }
    cfg
}

fn parse_mainnet(args: &[String]) -> MainnetConfig {
    let mut cfg = MainnetConfig::default();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--snapshot" => cfg.snapshot.push(next_value(args, &mut i, "--snapshot").into()),
            "--snapshot-block" => cfg.snapshot_block = Some(parse_next(args, &mut i, "--snapshot-block")),
            "--snapshot-accounts" => cfg.snapshot_accounts = Some(parse_next(args, &mut i, "--snapshot-accounts")),
            "--state" => cfg.state = Some(next_value(args, &mut i, "--state").into()),
            "--partial" => {
                cfg.partial = true;
                i += 1;
            }
            "--partial-capacity" => cfg.partial_capacity = parse_next(args, &mut i, "--partial-capacity"),
            "--feed-url" => cfg.feed_url = next_value(args, &mut i, "--feed-url"),
            "--confirm-url" => cfg.confirm_url = next_value(args, &mut i, "--confirm-url"),
            "--rpc-port" => cfg.rpc_port = parse_next(args, &mut i, "--rpc-port"),
            "--pir-port" => cfg.pir_port = parse_next(args, &mut i, "--pir-port"),
            "--proxy-upstream" => cfg.proxy_upstream = Some(next_value(args, &mut i, "--proxy-upstream")),
            "--reconcile-every" => cfg.reconcile_every = parse_next(args, &mut i, "--reconcile-every"),
            "--reconcile-samples" => cfg.reconcile_samples = parse_next(args, &mut i, "--reconcile-samples"),
            "--lwe-dim" => cfg.lwe_dim = Some(parse_next(args, &mut i, "--lwe-dim")),
            "--help" | "-h" => {
                print_usage();
                std::process::exit(0);
            }
            other => unknown(other),
        }
    }
    cfg
}

/// Reads and parses the value following flag `name`, advancing past both.
fn parse_next<T: std::str::FromStr>(args: &[String], i: &mut usize, name: &str) -> T {
    let v = next_value(args, i, name);
    v.parse().unwrap_or_else(|_| {
        eprintln!("risepir-rpc: {name} got an unparseable value {v:?}");
        std::process::exit(2);
    })
}

/// Reads the value following flag `name` (i.e. `args[*i + 1]`), advancing
/// `*i` past both the flag and its value — or exits with a usage error.
fn next_value(args: &[String], i: &mut usize, name: &str) -> String {
    let Some(v) = args.get(*i + 1) else {
        eprintln!("risepir-rpc: {name} requires a value");
        std::process::exit(2);
    };
    let v = v.clone();
    *i += 2;
    v
}

fn unknown(flag: &str) -> ! {
    eprintln!("risepir-rpc: unknown argument: {flag}");
    print_usage();
    std::process::exit(2);
}

fn print_usage() {
    eprintln!("usage:");
    eprintln!("  risepir-rpc mock    [--chain-id <u64>] [--rpc-port <u16>] [--pir-port <u16>] [--proxy-upstream <url>]");
    eprintln!("  risepir-rpc mainnet [--snapshot <csv[.gz]>]... [--snapshot-block <N>] [--snapshot-accounts <N>]");
    eprintln!("                      [--state <file>] [--partial] [--partial-capacity <N>]");
    eprintln!("                      [--feed-url <url>] [--confirm-url <url>]");
    eprintln!("                      [--rpc-port <u16>] [--pir-port <u16>] [--proxy-upstream <url>]");
    eprintln!("                      [--reconcile-every <blocks>] [--reconcile-samples <N>] [--lwe-dim <N>]");
    eprintln!();
    eprintln!("mainnet needs one data source: --snapshot (+ --snapshot-block), --state, or --partial.");
    eprintln!("See docs/deploy.md for the full runbook.");
}

/// The loud, multi-line startup warning `docs/plan.md` ADR-0012 requires
/// whenever `--proxy-upstream` is set: this is the one configuration that
/// makes this deployment leak exactly the information PIR exists to hide,
/// so the trade must be impossible to miss at the moment it is taken.
fn print_proxy_warning(url: &str) {
    eprintln!("################################################################");
    eprintln!("# WARNING: --proxy-upstream is set to: {url}");
    eprintln!("#");
    eprintln!("# Every JSON-RPC method OTHER than eth_getBalance / eth_chainId /");
    eprintln!("# net_version / eth_blockNumber will now be forwarded VERBATIM to");
    eprintln!("# that upstream node. This LEAKS exactly the information private");
    eprintln!("# eth_getBalance exists to hide: e.g. eth_call / eth_getLogs /");
    eprintln!("# eth_sendRawTransaction reveal to the upstream operator which");
    eprintln!("# account(s) you are interested in.");
    eprintln!("#");
    eprintln!("# eth_getBalance itself always stays private, proxy or not.");
    eprintln!("#");
    eprintln!("# Only run with --proxy-upstream if you understand and accept");
    eprintln!("# this trade-off (docs/plan.md ADR-0012).");
    eprintln!("################################################################");
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
