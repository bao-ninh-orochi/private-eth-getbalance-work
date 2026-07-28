//! `risepir-rpc` binary — two deployments behind one JSON-RPC front end:
//!
//! ```text
//! risepir-rpc mock    [--chain-id <u64>] [--rpc-port <u16>] [--pir-port <u16>] [--proxy-upstream <url>]
//!                     [--web <dir>]
//! risepir-rpc mainnet [--snapshot <csv[.gz]>]... [--snapshot-block <N>] [--snapshot-accounts <N>]
//!                     [--snapshot-rewind <N>] [--snapshot-audit-samples <N>]
//!                     [--hard-refresh <file>] [--refresh-url <url>]...
//!                     [--state <file>] [--save-interval <secs>] [--partial] [--partial-capacity <N>]
//!                     [--feed-url <url>]... [--confirm-url <url>]
//!                     [--rpc-port <u16>] [--pir-port <u16>] [--proxy-upstream <url>]
//!                     [--reconcile-every <blocks>] [--reconcile-samples <N>] [--lwe-dim <N>]
//!                     [--web <dir>]
//! ```
//!
//! `--snapshot-rewind`/`--snapshot-audit-samples`/`--hard-refresh` are the
//! three ADR-0040 mechanisms for `docs/deploy.md` §2.1's finding that the
//! BigQuery snapshot export is measurably not exact at its own declared
//! block: the first narrows the bootstrap's exposure by re-deriving a
//! window from the chain itself, the second measures and discloses
//! whatever residual error remains, and the third is the general-purpose
//! quorum-verified correction tool for any known-suspect address list.
//!
//! `mock` is the Stage-0 demo (synthetic chain, small LWE dim, seeded
//! demo accounts). `mainnet` is the real thing (`docs/deploy.md`):
//! snapshot/state/partial bootstrap, live `finalized` follow over public
//! RPC, cross-provider reconciliation, default LWE parameters.

use risepir_rpc::demo::{self, DemoConfig};
use risepir_rpc::front::{self, FrontConfig};
use risepir_rpc::mainnet::{self, MainnetConfig};

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("mock") => run_mock(parse_mock(&args[2..])).await,
        Some("mainnet") => run_mainnet(parse_mainnet(&args[2..])).await,
        Some("client") => run_client(parse_client(&args[2..])).await,
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
    if handle.web_served {
        println!("  Web front end:      http://{}/   <- open this in a browser", handle.pir_addr);
    }
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
    let save_interval = cfg.save_interval_secs;
    let save_interval_explicit = cfg.save_interval_explicit;
    let journal_restore = cfg.journal_restore;
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
    if handle.web_served {
        println!("  Web front end:      http://{}/   <- open this in a browser", handle.pir_addr);
    }
    println!();
    println!("Try:");
    println!("  cast balance <address> --rpc-url http://{}", handle.rpc_addr);
    println!("  cast block-number --rpc-url http://{}", handle.rpc_addr);
    if state_path.is_some() {
        println!();
        if save_interval > 0 {
            // ADR-0037: the effective value always prints, and whether it
            // came from the operator or from --journal-restore's own
            // setting — a silent default is exactly what made deploy.md
            // and CLAUDE.md go stale the first time this coupling shipped.
            let provenance = if save_interval_explicit {
                "explicit --save-interval".to_string()
            } else {
                format!(
                    "default for --journal-restore {} — pass --save-interval to override",
                    if journal_restore { "on" } else { "off" }
                )
            };
            println!("State autosave: every {save_interval}s while following ({provenance}; 0 disables).");
            println!("Ctrl-C also saves state before exiting.");
        } else {
            println!("State autosave: DISABLED (--save-interval 0). Ctrl-C still saves state before exiting.");
        }
    }

    shutdown_signal().await;
    // The final save goes through the same StateSaver the follow loop's
    // periodic saves use (ADR-0025): its mutex is what stops this save
    // from writing `<path>.tmp` concurrently with an in-flight autosave —
    // interleaved writers would produce garbage and rename it over the
    // previous good file. `save_now` waits for any running save, then
    // always writes the current state.
    if let Some(saver) = &handle.saver {
        eprintln!("risepir-rpc mainnet: shutdown signal — saving state ...");
        match saver.save_now(&handle.node, "shutdown").await {
            Ok(_) => eprintln!("risepir-rpc mainnet: state saved; exiting"),
            Err(e) => eprintln!("risepir-rpc mainnet: WARNING: state save failed: {e}"),
        }
    }
    std::process::exit(0);
}

/// Resolves on SIGINT (Ctrl-C) *or* — on unix — SIGTERM. Handling SIGTERM
/// matters because it is what `systemd`, `docker stop`, and most process
/// managers send by default: without this, a bare SIGTERM would kill the
/// process with **no state save**, silently discarding the fast-restart
/// file (the systemd unit's `KillSignal=SIGINT` papers over it, but the
/// binary should not depend on every supervisor being configured that
/// way).
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler");
        tokio::select! {
            r = tokio::signal::ctrl_c() => r.expect("install Ctrl-C handler"),
            _ = sigterm.recv() => {}
        }
    }
    #[cfg(not(unix))]
    tokio::signal::ctrl_c().await.expect("install Ctrl-C handler");
}

async fn run_client(cfg: FrontConfig) {
    if let Some(url) = &cfg.proxy_upstream {
        print_proxy_warning(url);
    }
    let pir_url = cfg.pir_url.clone();
    let handle = front::spawn(cfg).await;

    println!("RisePIR private eth_getBalance — remote front end");
    println!("  PIR server:  {pir_url}");
    println!("  JSON-RPC:    http://{}   (local — point your wallet here)", handle.rpc_addr);
    println!(
        "  data set:    {}",
        if handle.complete {
            "COMPLETE nonzero-balance set (not-found answers 0x0)"
        } else {
            "PARTIAL (only accounts touched since the server's bootstrap; not-found ERRORS)"
        }
    );
    println!("  hint pinned: block {} (the rewind serves the server's head regardless)", handle.pinned_block);
    println!();
    println!("The queried address NEVER leaves this machine — the server sees only LWE query vectors.");
    println!();
    println!("Try:  cast balance <address> --rpc-url http://{}", handle.rpc_addr);

    std::future::pending::<()>().await;
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
            "--bind" => cfg.bind = parse_next(args, &mut i, "--bind"),
            "--proxy-upstream" => cfg.proxy_upstream = Some(next_value(args, &mut i, "--proxy-upstream")),
            "--web" => cfg.web_dir = Some(next_value(args, &mut i, "--web").into()),
            "--help" | "-h" => {
                print_usage();
                std::process::exit(0);
            }
            other => unknown(other),
        }
    }
    cfg
}

fn parse_client(args: &[String]) -> FrontConfig {
    let mut cfg = FrontConfig::default();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--pir-url" => cfg.pir_url = next_value(args, &mut i, "--pir-url"),
            "--rpc-port" => cfg.rpc_port = parse_next(args, &mut i, "--rpc-port"),
            "--bind" => cfg.bind = parse_next(args, &mut i, "--bind"),
            "--chain-id" => cfg.chain_id = parse_next(args, &mut i, "--chain-id"),
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
    // `--feed-url` is repeatable and ordered. The first occurrence
    // *replaces* the built-in list rather than appending to it — an
    // operator naming their own endpoint must not silently keep ours
    // ahead of or behind it — and subsequent occurrences append, building
    // the fallback chain left to right.
    let mut feed_url_seen = false;
    // Tracked separately from `cfg.save_interval_secs` itself so the
    // default can be resolved *after* every flag has been read (ADR-0037):
    // the effective default depends on where `cfg.journal_restore` ends
    // up, and that flag may appear before or after `--save-interval` on
    // the command line — resolving eagerly would make the two flags'
    // relative order matter, which they must not.
    let mut save_interval_explicit = false;
    // `--refresh-url` (ADR-0040) follows the identical replace-then-append
    // convention as `--feed-url` above, for the identical reason.
    let mut refresh_url_seen = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--snapshot" => cfg.snapshot.push(next_value(args, &mut i, "--snapshot").into()),
            "--snapshot-block" => cfg.snapshot_block = Some(parse_next(args, &mut i, "--snapshot-block")),
            "--snapshot-accounts" => cfg.snapshot_accounts = Some(parse_next(args, &mut i, "--snapshot-accounts")),
            "--snapshot-rewind" => cfg.snapshot_rewind = parse_next(args, &mut i, "--snapshot-rewind"),
            "--snapshot-audit-samples" => cfg.snapshot_audit_samples = parse_next(args, &mut i, "--snapshot-audit-samples"),
            "--hard-refresh" => cfg.hard_refresh = Some(next_value(args, &mut i, "--hard-refresh").into()),
            "--refresh-url" => {
                let url = next_value(args, &mut i, "--refresh-url");
                if !refresh_url_seen {
                    cfg.refresh_urls.clear();
                    refresh_url_seen = true;
                }
                cfg.refresh_urls.push(url);
            }
            "--state" => cfg.state = Some(next_value(args, &mut i, "--state").into()),
            "--save-interval" => {
                cfg.save_interval_secs = parse_next(args, &mut i, "--save-interval");
                save_interval_explicit = true;
            }
            "--journal-restore" => {
                // Kept accepted, and bare — a no-op now that this is the
                // default (ADR-0037) — for scripts that already pass it
                // and for operators who want to say so explicitly.
                cfg.journal_restore = true;
                i += 1;
            }
            "--no-journal-restore" => {
                cfg.journal_restore = false;
                i += 1;
            }
            "--partial" => {
                cfg.partial = true;
                i += 1;
            }
            "--partial-capacity" => cfg.partial_capacity = parse_next(args, &mut i, "--partial-capacity"),
            "--feed-url" => {
                let url = next_value(args, &mut i, "--feed-url");
                if !feed_url_seen {
                    cfg.feed_urls.clear();
                    feed_url_seen = true;
                }
                cfg.feed_urls.push(url);
            }
            "--confirm-url" => cfg.confirm_url = next_value(args, &mut i, "--confirm-url"),
            "--rpc-port" => cfg.rpc_port = parse_next(args, &mut i, "--rpc-port"),
            "--pir-port" => cfg.pir_port = parse_next(args, &mut i, "--pir-port"),
            "--bind" => cfg.bind = parse_next(args, &mut i, "--bind"),
            "--proxy-upstream" => cfg.proxy_upstream = Some(next_value(args, &mut i, "--proxy-upstream")),
            "--reconcile-every" => cfg.reconcile_every = parse_next(args, &mut i, "--reconcile-every"),
            "--reconcile-samples" => {
                cfg.reconcile_samples = parse_next(args, &mut i, "--reconcile-samples");
                // 0 would make every non-empty block a zero-attempt "Dark"
                // checkpoint (violating that variant's own >= 1 invariant)
                // and escalate a false "provider unavailable" CRITICAL at
                // the threshold — refuse it and name the real off switch.
                if cfg.reconcile_samples == 0 {
                    eprintln!(
                        "risepir-rpc: --reconcile-samples must be >= 1; to disable reconciliation use --reconcile-every 0"
                    );
                    std::process::exit(2);
                }
            }
            "--lwe-dim" => cfg.lwe_dim = Some(parse_next(args, &mut i, "--lwe-dim")),
            "--web" => cfg.web_dir = Some(next_value(args, &mut i, "--web").into()),
            "--help" | "-h" => {
                print_usage();
                std::process::exit(0);
            }
            other => unknown(other),
        }
    }
    // Resolve --save-interval's default only now that every flag has been
    // read (ADR-0037): --journal-restore/--no-journal-restore may have
    // appeared before or after --save-interval, in either order, with
    // identical results either way. An explicit --save-interval always
    // wins over the default, whichever way --journal-restore ended up.
    cfg.save_interval_explicit = save_interval_explicit;
    if !save_interval_explicit {
        cfg.save_interval_secs = if cfg.journal_restore { 21_600 } else { 1_800 };
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
    eprintln!("  risepir-rpc mock    [--chain-id <u64>] [--rpc-port <u16>] [--pir-port <u16>] [--bind <ip>] [--proxy-upstream <url>]");
    eprintln!("                      [--web <dir>]");
    eprintln!("  risepir-rpc mainnet [--snapshot <csv[.gz]>]... [--snapshot-block <N>] [--snapshot-accounts <N>]");
    eprintln!("                      [--snapshot-rewind <N>] [--snapshot-audit-samples <N>]");
    eprintln!("                      [--hard-refresh <file>] [--refresh-url <url>]...");
    eprintln!("                      [--state <file>] [--save-interval <secs>]");
    eprintln!("                      [--journal-restore] [--no-journal-restore]");
    eprintln!("                      [--partial] [--partial-capacity <N>]");
    eprintln!("                      [--feed-url <url>]... [--confirm-url <url>]");
    eprintln!("                      [--rpc-port <u16>] [--pir-port <u16>] [--bind <ip>] [--proxy-upstream <url>]");
    eprintln!("                      [--reconcile-every <blocks>] [--reconcile-samples <N>] [--lwe-dim <N>] [--web <dir>]");
    eprintln!("  risepir-rpc client  --pir-url <http://server:8645> [--rpc-port <u16>] [--bind <ip>]");
    eprintln!("                      [--chain-id <u64>] [--proxy-upstream <url>]");
    eprintln!();
    eprintln!("mainnet needs one data source: --snapshot (+ --snapshot-block), --state, or --partial.");
    eprintln!("--state also always writes a <state>.journal delta sidecar once a first full save exists");
    eprintln!("(ADR-0026): one small per-block delta, appended and fsynced as each block applies.");
    eprintln!("--journal-restore (default ON, ADR-0037) replays it at startup, resuming above the last");
    eprintln!("full save instead of at it; --no-journal-restore turns that off, falling back to only");
    eprintln!("*scanning* and reporting the journal (`journal intact: N records ...`, the original");
    eprintln!("ADR-0026 soak signal) without replaying it. --journal-restore itself stays accepted as a");
    eprintln!("bare flag — a no-op now that it is the default — for scripts and explicitness.");
    eprintln!("--save-interval (0 = off) bounds how far the --state file can fall behind the running");
    eprintln!("server: the follow loop rewrites it that many seconds after the previous save finished.");
    eprintln!("Its default is coupled to --journal-restore (ADR-0037): 21600 (6h) when restore is on,");
    eprintln!("since the journal then bounds an ungraceful kill's replay cost, not the full save; 1800");
    eprintln!("(30 min, ADR-0025) when restore is off, since the full save is what bounds it there. An");
    eprintln!("explicit --save-interval always wins over either default.");
    eprintln!("--snapshot-rewind (default 2000, 0 disables) treats the snapshot as exact N blocks before");
    eprintln!("--snapshot-block instead of exactly at it, so the ordinary replay re-derives the rewind");
    eprintln!("window from the chain's own absolute post-state (ADR-0040) — narrows the boundary error,");
    eprintln!("does not close it, and does not fix relative withdrawal credits (--hard-refresh does).");
    eprintln!("--snapshot-audit-samples (default 512, 0 disables) reservoir-samples that many addresses");
    eprintln!("during ingest and verifies them against --refresh-url's quorum after setup, reporting a");
    eprintln!("measured disagreement rate (with a Wilson 95% CI) instead of assuming the export is exact.");
    eprintln!("--hard-refresh <file> quorum-verifies a newline-delimited address list against --refresh-url");
    eprintln!("(repeatable; default two independent providers) and corrects the store only where every");
    eprintln!("configured provider agrees on a value differing from what is stored — runs in the background,");
    eprintln!("never blocking serving or following; corrections drain into blocks 2000 at a time (ADR-0040).");
    eprintln!("client runs the JSON-RPC front end + rewind client on THIS machine against a remote");
    eprintln!("PIR server (started with --bind 0.0.0.0) — the queried address never leaves this machine.");
    eprintln!("--web <dir> serves the browser front end (ADR-0019) on the PIR port: the same rewind");
    eprintln!("client, compiled to wasm and running in the page, so the address never leaves the browser.");
    eprintln!("Build its wasm first: cargo run -p xtask --release -- web");
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

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_string()).collect()
    }

    /// The built-in feed list is ordered and has a fallback: a lone
    /// endpoint wedges the follow loop permanently the first time a
    /// provider refuses one heavy block on a plan limit (see
    /// `RpcFeed::new_multi`).
    #[test]
    fn default_feed_list_is_ordered_with_a_fallback() {
        let cfg = parse_mainnet(&args(&["--partial"]));
        assert!(
            cfg.feed_urls.len() >= 2,
            "a single default endpoint is a permanent single point of failure"
        );
        assert_eq!(cfg.feed_urls[0], "https://eth.drpc.org", "primary must stay dRPC");
    }

    /// One `--feed-url` must *replace* the built-ins, not prepend to or
    /// append to them: an operator naming an endpoint gets exactly that
    /// endpoint, with no surprise third party still in the chain.
    #[test]
    fn one_feed_url_replaces_the_defaults() {
        let cfg = parse_mainnet(&args(&["--partial", "--feed-url", "https://example.test/rpc"]));
        assert_eq!(cfg.feed_urls, vec!["https://example.test/rpc".to_string()]);
    }

    /// Repeats build the fallback chain left to right.
    #[test]
    fn repeated_feed_urls_build_the_chain_in_order() {
        let cfg = parse_mainnet(&args(&[
            "--partial",
            "--feed-url",
            "https://a.test",
            "--feed-url",
            "https://b.test",
            "--feed-url",
            "https://c.test",
        ]));
        assert_eq!(
            cfg.feed_urls,
            vec![
                "https://a.test".to_string(),
                "https://b.test".to_string(),
                "https://c.test".to_string()
            ]
        );
    }

    /// `--save-interval` parses explicit values, including the `0`
    /// (disable) sentinel — an accidental change here would silently
    /// reintroduce the unbounded-staleness behavior this flag exists to
    /// bound. What it defaults to when *omitted* is coupled to
    /// `--journal-restore` since ADR-0037 — see
    /// `save_interval_default_is_coupled_to_journal_restore_but_explicit_always_wins`.
    #[test]
    fn save_interval_parses_explicit_values_including_zero() {
        assert_eq!(
            parse_mainnet(&args(&["--partial", "--save-interval", "60"])).save_interval_secs,
            60
        );
        assert_eq!(
            parse_mainnet(&args(&["--partial", "--save-interval", "0"])).save_interval_secs,
            0
        );
    }

    /// `--journal-restore` is a bare flag, default **on** (ADR-0037 flips
    /// ADR-0026's original opt-in-behind-a-soak default now that the soak
    /// evidence has held up). Kept accepted — a no-op now — for scripts
    /// that already pass it and for explicitness.
    #[test]
    fn journal_restore_is_a_bare_flag_defaulting_on() {
        assert!(parse_mainnet(&args(&["--partial"])).journal_restore);
        assert!(parse_mainnet(&args(&["--partial", "--journal-restore"])).journal_restore);
        // Must not consume a following value as its own argument.
        let cfg = parse_mainnet(&args(&["--journal-restore", "--partial-capacity", "42"]));
        assert!(cfg.journal_restore);
        assert_eq!(cfg.partial_capacity, 42);
    }

    /// `--no-journal-restore` is the new off switch (ADR-0037) — also a
    /// bare flag, consuming no value, exactly like `--journal-restore`.
    #[test]
    fn no_journal_restore_is_a_bare_flag_that_turns_it_off() {
        assert!(!parse_mainnet(&args(&["--partial", "--no-journal-restore"])).journal_restore);
        // Must not consume a following value as its own argument either.
        let cfg = parse_mainnet(&args(&["--no-journal-restore", "--partial-capacity", "42"]));
        assert!(!cfg.journal_restore);
        assert_eq!(cfg.partial_capacity, 42);
    }

    /// `--save-interval`'s default is coupled to `--journal-restore`
    /// (ADR-0037): restore on (the new default) widens it to 6 h, since
    /// the journal — not the full save — now bounds replay after an
    /// ungraceful kill; restore off keeps ADR-0025's original 30 min. An
    /// explicit `--save-interval` always wins over either default, and
    /// the resolution must not depend on which of the two flags appears
    /// first on the command line. All four combinations pinned here.
    #[test]
    fn save_interval_default_is_coupled_to_journal_restore_but_explicit_always_wins() {
        // 1. journal-restore ON (default) + no explicit --save-interval -> 6h.
        let cfg = parse_mainnet(&args(&["--partial"]));
        assert!(cfg.journal_restore);
        assert!(!cfg.save_interval_explicit);
        assert_eq!(cfg.save_interval_secs, 21_600);

        // 2. journal-restore OFF + no explicit --save-interval -> 30 min.
        let cfg = parse_mainnet(&args(&["--partial", "--no-journal-restore"]));
        assert!(!cfg.journal_restore);
        assert!(!cfg.save_interval_explicit);
        assert_eq!(cfg.save_interval_secs, 1_800);

        // 3. journal-restore ON + explicit --save-interval -> explicit wins.
        let cfg = parse_mainnet(&args(&["--partial", "--save-interval", "300"]));
        assert!(cfg.journal_restore);
        assert!(cfg.save_interval_explicit);
        assert_eq!(cfg.save_interval_secs, 300);

        // 4. journal-restore OFF + explicit --save-interval -> explicit wins.
        let cfg = parse_mainnet(&args(&["--partial", "--no-journal-restore", "--save-interval", "300"]));
        assert!(!cfg.journal_restore);
        assert!(cfg.save_interval_explicit);
        assert_eq!(cfg.save_interval_secs, 300);

        // Order must not matter: --save-interval given *before*
        // --no-journal-restore must resolve identically to combination 4.
        let cfg = parse_mainnet(&args(&["--partial", "--save-interval", "300", "--no-journal-restore"]));
        assert!(!cfg.journal_restore);
        assert!(cfg.save_interval_explicit);
        assert_eq!(cfg.save_interval_secs, 300);
    }

    /// The repeatable flag must not have disturbed its neighbours.
    #[test]
    fn confirm_url_is_still_single_valued() {
        let cfg = parse_mainnet(&args(&[
            "--partial",
            "--feed-url",
            "https://a.test",
            "--confirm-url",
            "https://independent.test",
        ]));
        assert_eq!(cfg.confirm_url, "https://independent.test");
        assert_eq!(cfg.feed_urls, vec!["https://a.test".to_string()]);
    }

    // ── ADR-0040: --hard-refresh / --refresh-url / --snapshot-rewind /
    // --snapshot-audit-samples ──────────────────────────────────────────

    /// `--hard-refresh` is unset by default, and takes exactly one path
    /// value.
    #[test]
    fn hard_refresh_defaults_off_and_parses_a_path() {
        assert!(parse_mainnet(&args(&["--partial"])).hard_refresh.is_none());
        let cfg = parse_mainnet(&args(&["--partial", "--hard-refresh", "addresses.txt"]));
        assert_eq!(cfg.hard_refresh, Some(std::path::PathBuf::from("addresses.txt")));
    }

    /// The built-in default is two distinct providers — the floor
    /// `hard_refresh::validate_refresh_urls` requires, satisfied without
    /// any flag at all.
    #[test]
    fn default_refresh_urls_are_two_distinct_providers() {
        let cfg = parse_mainnet(&args(&["--partial"]));
        assert_eq!(cfg.refresh_urls.len(), 2);
        assert_ne!(cfg.refresh_urls[0], cfg.refresh_urls[1]);
    }

    /// `--refresh-url` follows the identical replace-then-append
    /// convention `--feed-url` already established: the first occurrence
    /// replaces the built-in pair, later occurrences extend it.
    #[test]
    fn refresh_url_replaces_defaults_then_appends() {
        let cfg = parse_mainnet(&args(&["--partial", "--refresh-url", "https://a.test"]));
        assert_eq!(cfg.refresh_urls, vec!["https://a.test".to_string()]);

        let cfg = parse_mainnet(&args(&[
            "--partial",
            "--refresh-url",
            "https://a.test",
            "--refresh-url",
            "https://b.test",
        ]));
        assert_eq!(cfg.refresh_urls, vec!["https://a.test".to_string(), "https://b.test".to_string()]);
    }

    /// `--snapshot-rewind` parses and defaults to 2000 (ADR-0040) — an
    /// accidental `0` default would silently disable the mitigation.
    #[test]
    fn snapshot_rewind_parses_and_defaults_to_2000() {
        assert_eq!(parse_mainnet(&args(&["--partial"])).snapshot_rewind, 2_000);
        assert_eq!(
            parse_mainnet(&args(&["--partial", "--snapshot-rewind", "500"])).snapshot_rewind,
            500
        );
        assert_eq!(
            parse_mainnet(&args(&["--partial", "--snapshot-rewind", "0"])).snapshot_rewind,
            0
        );
    }

    /// `--snapshot-audit-samples` parses and defaults to 512 (ADR-0040).
    #[test]
    fn snapshot_audit_samples_parses_and_defaults_to_512() {
        assert_eq!(parse_mainnet(&args(&["--partial"])).snapshot_audit_samples, 512);
        assert_eq!(
            parse_mainnet(&args(&["--partial", "--snapshot-audit-samples", "1000"])).snapshot_audit_samples,
            1_000
        );
        assert_eq!(
            parse_mainnet(&args(&["--partial", "--snapshot-audit-samples", "0"])).snapshot_audit_samples,
            0
        );
    }
}
