//! `risepir-rpc` binary: boots the Stage 0.4 demo deployment
//! ([`risepir_rpc::demo`]) — a mock-backed RisePIR server plus the private
//! JSON-RPC front end on top — and serves both forever.
//!
//! ```text
//! risepir-rpc [--chain-id <u64>] [--rpc-port <u16>] [--pir-port <u16>] [--proxy-upstream <url>]
//! ```

use risepir_rpc::demo::{self, DemoConfig};

#[tokio::main]
async fn main() {
    let cfg = parse_args();

    if let Some(url) = &cfg.proxy_upstream {
        print_proxy_warning(url);
    }

    let handle = demo::spawn(cfg).await;

    println!("RisePIR private eth_getBalance — Stage 0.4 demo");
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

    // Every actual server (PIR HTTP, JSON-RPC, the follow loop) is
    // already running as a detached `tokio` task inside `demo::spawn` —
    // this just keeps the process (and therefore the runtime driving
    // those tasks) alive.
    std::future::pending::<()>().await;
}

/// Minimal hand-rolled flag parsing (no `clap`): this binary has exactly
/// four optional flags, each taking one value, so a `clap` dependency
/// would buy little beyond what `std::env::args` + a small loop already
/// gives directly.
fn parse_args() -> DemoConfig {
    let mut cfg = DemoConfig::default();
    let args: Vec<String> = std::env::args().collect();

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--chain-id" => {
                let v = next_value(&args, &mut i, "--chain-id");
                cfg.chain_id = v.parse().unwrap_or_else(|_| {
                    eprintln!("risepir-rpc: --chain-id must be a u64, got {v:?}");
                    std::process::exit(2);
                });
            }
            "--rpc-port" => {
                let v = next_value(&args, &mut i, "--rpc-port");
                cfg.rpc_port = v.parse().unwrap_or_else(|_| {
                    eprintln!("risepir-rpc: --rpc-port must be a u16, got {v:?}");
                    std::process::exit(2);
                });
            }
            "--pir-port" => {
                let v = next_value(&args, &mut i, "--pir-port");
                cfg.pir_port = v.parse().unwrap_or_else(|_| {
                    eprintln!("risepir-rpc: --pir-port must be a u16, got {v:?}");
                    std::process::exit(2);
                });
            }
            "--proxy-upstream" => {
                cfg.proxy_upstream = Some(next_value(&args, &mut i, "--proxy-upstream"));
            }
            "--help" | "-h" => {
                print_usage();
                std::process::exit(0);
            }
            other => {
                eprintln!("risepir-rpc: unknown argument: {other}");
                print_usage();
                std::process::exit(2);
            }
        }
    }
    cfg
}

/// Reads the value following flag `name` (i.e. `args[*i + 1]`), advancing
/// `*i` past both the flag and its value — or exits with a usage error if
/// no value follows. A free function taking `&mut usize` rather than a
/// closure over `i`, so there is no capture ambiguity between mutating
/// `i` and reading `args` at the call sites above.
fn next_value(args: &[String], i: &mut usize, name: &str) -> String {
    let Some(v) = args.get(*i + 1) else {
        eprintln!("risepir-rpc: {name} requires a value");
        std::process::exit(2);
    };
    let v = v.clone();
    *i += 2;
    v
}

fn print_usage() {
    eprintln!("usage: risepir-rpc [--chain-id <u64>] [--rpc-port <u16>] [--pir-port <u16>] [--proxy-upstream <url>]");
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
