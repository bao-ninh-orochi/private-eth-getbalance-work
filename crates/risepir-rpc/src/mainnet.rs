//! The real-mainnet deployment (Stage 1): bootstrap from a snapshot /
//! state file / empty-partial, follow `finalized` through
//! [`risepir_feed::rpc::RpcFeed`], reconcile against an independent
//! provider, and serve the same private JSON-RPC front end the demo does
//! — at real LWE parameters (`SimpleConfig::default()`, `lwe_dim` 1275).
//!
//! # The three bootstrap sources, in precedence order
//!
//! 1. **State file** (`--state`, file exists): reassembled via
//!    [`crate::state::load`] — bit-identical `A`/hints to the run that
//!    saved it, so previously bootstrapped PIR clients stay valid; the
//!    follow loop replays `saved_block+1 ..= finalized` through the feed.
//! 2. **Snapshot** (`--snapshot`, BigQuery balances export): streamed
//!    into a fresh store (ADR-0016), geometry sized from
//!    `--snapshot-accounts` (the `bq` gate's count) or a counting
//!    pre-pass; the set is **complete**, so `NotFound ⇒ 0x0` is exact
//!    (ADR-0015).
//! 3. **Partial** (`--partial`, no snapshot): starts *empty* at the
//!    current finalized block and tracks only accounts the chain touches
//!    from then on. Honesty rules differ (`docs/deploy.md`):
//!    - `NotFound` is answered with a JSON-RPC **error**, never `0x0` —
//!      absence only means zero for a complete set;
//!    - withdrawal credits to *untracked* recipients are **skipped** (the
//!      store has no true prior to add to; crediting `0 + amount` would
//!      fabricate a balance). Recipients stay untracked — and therefore
//!      erroring, never wrong — until a transaction touches them.
//!
//! # Never-wrong-answer posture of the follow loop
//!
//! - Transient RPC failures retry forever (same finalized block —
//!   idempotent); the server keeps serving its last applied block,
//!   labelled, meanwhile.
//! - Any [`risepir_server::ServerError`] from `apply_block`, and any
//!   reconciliation **value mismatch**, permanently stops the follow loop
//!   (serving continues at the last good block) with a `CRITICAL` log —
//!   the operator re-bootstraps. Reconciliation *fetch* failures merely
//!   skip that sample (an unreachable reference provider must not take
//!   the service down; only evidence of drift may).

use std::collections::HashSet;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;
use std::time::Duration;

use ikpir_common::{SimpleConfig, SimplePirBackend};
use risepir_client::RisePirClient;
use risepir_feed::rpc::{FetchedBlock, RpcClient, RpcFeed};
use risepir_feed::snapshot;
use risepir_http::{NodeState, PirHttpClient};
use risepir_proto::{keccak256, Backend, Geometry, ValueCodec};
use risepir_server::{DeltaRing, RisePirServer};
use segmented_cuckoo::Segmented3aryCuckooKVStore;

use crate::private_eth::PrivateEth;
use crate::state;

/// SCF geometry for the mainnet deployment — same shape the measured
/// numbers table used (`docs/numbers.md`: arity 3, `bucket_size` 4,
/// 32-bit fp, `key_tag(32) ‖ balance(96) ‖ checksum(16)`).
const ARITY: u32 = 3;
const BUCKET_SIZE: u32 = 4;
const FINGERPRINT_BITS: u32 = 32;

/// Poll cadence for `finalized` (it advances in ~32-block bursts every
/// ~6.4 min; polling faster than block time buys nothing).
const POLL_INTERVAL: Duration = Duration::from_secs(6);
/// Pause between retries after a transient feed error.
const RETRY_INTERVAL: Duration = Duration::from_secs(3);

pub(crate) fn value_codec() -> ValueCodec {
    ValueCodec {
        key_tag_bits: 32,
        balance_bits: 96,
        checksum_bits: 16,
    }
}

/// Knobs for [`spawn`]. Defaults are the free-tier deployment
/// `docs/deploy.md` describes.
#[derive(Clone, Debug)]
pub struct MainnetConfig {
    /// Address both listeners bind. Default loopback; `0.0.0.0` exposes
    /// them on all interfaces (remote deployment — see `docs/deploy.md`'s
    /// exposure guidance before doing this).
    pub bind: Ipv4Addr,
    /// JSON-RPC listen port (`0` = ephemeral).
    pub rpc_port: u16,
    /// PIR HTTP listen port (`0` = ephemeral).
    pub pir_port: u16,
    /// Feed endpoint — must serve `debug_traceBlockByNumber` +
    /// `prestateTracer`. dRPC's keyless endpoint does.
    pub feed_url: String,
    /// Independent reconciliation endpoint (recent-window `eth_getBalance`
    /// is enough). Should be a *different operator* than `feed_url`, so a
    /// lying/buggy feed is actually caught.
    pub confirm_url: String,
    /// Snapshot shards (`address,eth_balance` CSV / CSV.gz), in order.
    pub snapshot: Vec<PathBuf>,
    /// The block the snapshot's balances are exact at (from the `bq`
    /// gate). Required with `snapshot`.
    pub snapshot_block: Option<u64>,
    /// Nonzero-account count (from the `bq` gate) — sizes the geometry.
    /// Omitted ⇒ a counting pre-pass over the shards.
    pub snapshot_accounts: Option<u64>,
    /// State file: loaded at startup if it exists; written after a
    /// snapshot bootstrap and on Ctrl-C.
    pub state: Option<PathBuf>,
    /// Bootstrap empty at the current finalized block (see module docs).
    pub partial: bool,
    /// Geometry capacity for `--partial` (accounts). ~600 distinct
    /// touched accounts/block ⇒ the default covers multiple days.
    pub partial_capacity: u64,
    /// Opt-in proxy for non-private methods (ADR-0012).
    pub proxy_upstream: Option<String>,
    /// Reconcile every N applied blocks (`0` disables — not recommended).
    pub reconcile_every: u64,
    /// Sampled addresses per reconciliation.
    pub reconcile_samples: usize,
    /// Delta-ring retention, blocks.
    pub ring_capacity: usize,
    /// Override the LWE dimension (tests only — `None` means the real
    /// `SimpleConfig::default()`, `lwe_dim` 1275 / sigma 6.4).
    pub lwe_dim: Option<u32>,
}

impl Default for MainnetConfig {
    fn default() -> Self {
        Self {
            bind: Ipv4Addr::LOCALHOST,
            rpc_port: 8545,
            pir_port: 8645,
            feed_url: "https://eth.drpc.org".to_string(),
            confirm_url: "https://ethereum-rpc.publicnode.com".to_string(),
            snapshot: Vec::new(),
            snapshot_block: None,
            snapshot_accounts: None,
            state: None,
            partial: false,
            partial_capacity: 4_000_000,
            proxy_upstream: None,
            reconcile_every: 30,
            reconcile_samples: 8,
            ring_capacity: 600,
            lwe_dim: None,
        }
    }
}

/// What [`spawn`] hands back to `main` (and to tests).
pub struct MainnetHandle {
    /// Bound JSON-RPC address.
    pub rpc_addr: SocketAddr,
    /// Bound PIR HTTP address.
    pub pir_addr: SocketAddr,
    /// Whether this deployment serves a complete nonzero set
    /// (`NotFound ⇒ 0x0`) or a partial one (`NotFound ⇒ error`).
    pub complete: bool,
    /// The block the server was serving when `spawn` returned.
    pub head_at_start: u64,
    /// Shared node state — `main` uses it for the Ctrl-C state save.
    pub node: Arc<NodeState>,
}

/// Fatal deployment-configuration error: print and exit. Everything this
/// wraps happens before serving starts, so exiting is the honest move —
/// there is no traffic to keep alive.
fn die(msg: impl std::fmt::Display) -> ! {
    eprintln!("risepir-rpc mainnet: fatal: {msg}");
    std::process::exit(1);
}

/// Build and spawn the whole mainnet stack. Returns once the PIR
/// transport, JSON-RPC front end, and follow loop are all running.
pub async fn spawn(cfg: MainnetConfig) -> MainnetHandle {
    let codec = value_codec();
    let backend_config = match cfg.lwe_dim {
        Some(d) => SimpleConfig::with_lwe_dim(d),
        None => SimpleConfig::default(),
    };

    // The feed connects first: a mainnet deployment that cannot reach its
    // feed is dead on arrival, and the chain-id check must reject a
    // wrong-network endpoint before any state is touched (chain id 1).
    let feed = match RpcFeed::new(cfg.feed_url.clone(), 1).await {
        Ok(f) => f,
        Err(e) => die(format!("feed {}: {e}", cfg.feed_url)),
    };

    // ── Bootstrap: state file > snapshot > partial ─────────────────────
    let (server, complete) = if let Some(path) = cfg.state.as_ref().filter(|p| p.exists()) {
        if !cfg.snapshot.is_empty() {
            eprintln!(
                "risepir-rpc mainnet: note: state file {} exists; --snapshot is ignored \
                 (delete the state file to re-bootstrap from the snapshot)",
                path.display()
            );
        }
        eprintln!("risepir-rpc mainnet: loading state from {} ...", path.display());
        let started = std::time::Instant::now();
        match state::load(path, backend_config.clone(), &codec) {
            Ok(state::LoadedState { server, complete }) => {
                eprintln!(
                    "risepir-rpc mainnet: state loaded in {:.1}s — block {}, {} accounts, {}",
                    started.elapsed().as_secs_f64(),
                    server.block(),
                    server.num_items(),
                    if complete { "complete set" } else { "PARTIAL set" },
                );
                (server, complete)
            }
            Err(e) => die(format!("loading {}: {e}", path.display())),
        }
    } else if !cfg.snapshot.is_empty() {
        let snapshot_block = cfg
            .snapshot_block
            .unwrap_or_else(|| die("--snapshot requires --snapshot-block (the block the snapshot is exact at)"));
        let accounts = match cfg.snapshot_accounts {
            Some(n) => n,
            None => {
                eprintln!("risepir-rpc mainnet: counting snapshot rows (pass --snapshot-accounts to skip) ...");
                match snapshot::count_rows(&cfg.snapshot) {
                    Ok(n) => n,
                    Err(e) => die(e),
                }
            }
        };
        let geom = Geometry::for_accounts(accounts.max(1_000), ARITY, BUCKET_SIZE, FINGERPRINT_BITS, &codec, Backend::Simple)
            .unwrap_or_else(|e| die(format!("geometry for {accounts} accounts: {e}")));
        let sizes = geom.sizes(Backend::Simple, accounts);
        eprintln!(
            "risepir-rpc mainnet: geometry for {accounts} accounts: {} buckets, server DB {:.2} GB, load {:.3}",
            geom.num_buckets,
            sizes.server_db as f64 / 1e9,
            sizes.load_factor,
        );
        let mut store = Segmented3aryCuckooKVStore::new(
            geom.num_buckets,
            geom.bucket_size,
            geom.fingerprint_bits,
            geom.value_bits,
            geom.plaintext_bits,
        )
        .unwrap_or_else(|e| die(format!("store construction: {e:?}")));

        eprintln!("risepir-rpc mainnet: ingesting snapshot ({} shard(s)) ...", cfg.snapshot.len());
        let started = std::time::Instant::now();
        let mut ingested = 0u64;
        let stats = snapshot::ingest(&cfg.snapshot, |key, balance| {
            let encoded = codec.encode(&key, balance).map_err(|e| e.to_string())?;
            store.insert(key, &encoded).map_err(|e| format!("{e:?}"))?;
            ingested += 1;
            if ingested % 5_000_000 == 0 {
                eprintln!(
                    "risepir-rpc mainnet:   {ingested} accounts in {:.0}s ...",
                    started.elapsed().as_secs_f64()
                );
            }
            Ok(())
        })
        .unwrap_or_else(|e| die(e));
        eprintln!(
            "risepir-rpc mainnet: snapshot ingested in {:.0}s — {} rows, {} nonzero, {} zero skipped, max balance {} wei",
            started.elapsed().as_secs_f64(),
            stats.rows,
            stats.nonzero,
            stats.zero_skipped,
            stats.max_balance,
        );

        eprintln!("risepir-rpc mainnet: running PIR setup (one-time preprocessing) ...");
        let started = std::time::Instant::now();
        let server = RisePirServer::new(store, backend_config.clone(), codec, snapshot_block);
        eprintln!(
            "risepir-rpc mainnet: setup done in {:.1}s at block {snapshot_block}",
            started.elapsed().as_secs_f64()
        );

        if let Some(path) = &cfg.state {
            eprintln!("risepir-rpc mainnet: saving state to {} ...", path.display());
            if let Err(e) = state::save(&server, &codec, true, path) {
                // Non-fatal: the server is correct in memory; only restart
                // speed is lost. Say so and continue.
                eprintln!("risepir-rpc mainnet: WARNING: state save failed ({e}); continuing without");
            }
        }
        (server, true)
    } else if cfg.partial {
        let fin = match feed.finalized().await {
            Ok(f) => f,
            Err(e) => die(format!("fetching finalized block: {e}")),
        };
        let geom = Geometry::for_accounts(cfg.partial_capacity, ARITY, BUCKET_SIZE, FINGERPRINT_BITS, &codec, Backend::Simple)
            .unwrap_or_else(|e| die(format!("geometry: {e}")));
        let store = Segmented3aryCuckooKVStore::new(
            geom.num_buckets,
            geom.bucket_size,
            geom.fingerprint_bits,
            geom.value_bits,
            geom.plaintext_bits,
        )
        .unwrap_or_else(|e| die(format!("store construction: {e:?}")));
        eprintln!(
            "risepir-rpc mainnet: PARTIAL bootstrap at finalized block {fin} — empty set, capacity {} accounts.",
            cfg.partial_capacity
        );
        eprintln!(
            "risepir-rpc mainnet: partial mode serves only accounts touched from here on; everything else ERRORS (never 0x0)."
        );
        (RisePirServer::new(store, backend_config.clone(), codec, fin), false)
    } else {
        die("need a data source: --snapshot <csv[.gz]> --snapshot-block <N> (complete), or --state <file> (restart), or --partial (demo)");
    };

    let head_at_start = server.block();
    let node = Arc::new(NodeState::new(server, DeltaRing::new(cfg.ring_capacity), complete));

    // ── PIR HTTP transport ─────────────────────────────────────────────
    let pir_listener = tokio::net::TcpListener::bind((cfg.bind, cfg.pir_port))
        .await
        .unwrap_or_else(|e| die(format!("bind PIR port {}: {e}", cfg.pir_port)));
    let pir_addr = pir_listener.local_addr().expect("PIR local_addr");
    tokio::spawn({
        let router = NodeState::router(node.clone());
        async move {
            axum::serve(pir_listener, router).await.expect("PIR HTTP server crashed");
        }
    });

    // ── Follow loop ────────────────────────────────────────────────────
    tokio::spawn(follow_loop(
        feed,
        RpcClient::new(cfg.confirm_url.clone()),
        node.clone(),
        FollowConfig {
            complete,
            reconcile_every: cfg.reconcile_every,
            reconcile_samples: cfg.reconcile_samples,
            start_at: head_at_start,
        },
    ));

    // ── JSON-RPC front end (one in-process rewind client, ADR-0010) ───
    // Loopback even when bound to 0.0.0.0 — see demo.rs's identical note.
    let pir_client = PirHttpClient::new(crate::front::local_url(cfg.bind, pir_addr.port()));
    let setup_bundle = pir_client
        .setup()
        .await
        .unwrap_or_else(|e| die(format!("GET /setup from our own PIR transport: {e}")));
    let arity = setup_bundle.params.arity();
    let plaintext_bits = setup_bundle.params.plaintext_bits;
    let reshape_row_width_per_seg: Vec<u32> =
        setup_bundle.backend_params.iter().map(|sp| sp.reshape_row_width).collect();
    let pinned_block = setup_bundle.block;
    let rise_client: RisePirClient<SimplePirBackend> = RisePirClient::from_setup(setup_bundle, codec);

    let private_eth = Arc::new(PrivateEth {
        client: tokio::sync::Mutex::new(rise_client),
        pending_head: AtomicU64::new(pinned_block),
        pir: pir_client,
        reshape_row_width_per_seg,
        arity,
        plaintext_bits,
        chain_id: 1,
        strict_not_found: !complete,
        proxy_upstream: cfg.proxy_upstream.clone(),
        proxy_http: reqwest::Client::new(),
    });

    let rpc_listener = tokio::net::TcpListener::bind((cfg.bind, cfg.rpc_port))
        .await
        .unwrap_or_else(|e| die(format!("bind JSON-RPC port {}: {e}", cfg.rpc_port)));
    let rpc_addr = rpc_listener.local_addr().expect("RPC local_addr");
    tokio::spawn({
        let router = crate::rpc::router(private_eth);
        async move {
            axum::serve(rpc_listener, router).await.expect("JSON-RPC server crashed");
        }
    });

    MainnetHandle {
        rpc_addr,
        pir_addr,
        complete,
        head_at_start,
        node,
    }
}

struct FollowConfig {
    complete: bool,
    reconcile_every: u64,
    reconcile_samples: usize,
    start_at: u64,
}

/// The forever loop: poll `finalized`, apply each new block exactly once,
/// reconcile on cadence. See the module docs for the failure posture.
async fn follow_loop(feed: RpcFeed, confirm: RpcClient, node: Arc<NodeState>, cfg: FollowConfig) {
    let mut last = cfg.start_at;
    loop {
        let finalized = match feed.finalized().await {
            Ok(f) => f,
            Err(e) => {
                eprintln!("risepir-rpc mainnet: follow: finalized poll failed ({e}); retrying");
                tokio::time::sleep(RETRY_INTERVAL).await;
                continue;
            }
        };

        while last < finalized {
            let n = last + 1;
            let fetched = match feed.block_update(n).await {
                Ok(f) => f,
                Err(e) => {
                    eprintln!("risepir-rpc mainnet: follow: block {n} fetch failed ({e}); retrying");
                    tokio::time::sleep(RETRY_INTERVAL).await;
                    continue; // same n, idempotent
                }
            };
            let FetchedBlock {
                mut update,
                changed,
                credited,
            } = fetched;

            // Partial mode cannot honestly resolve a credit for an
            // account it has no prior for — see the module docs.
            if !cfg.complete && !update.credits.is_empty() {
                let changed_keys: HashSet<_> = update.changes.iter().map(|(k, _)| *k).collect();
                let mut kept = Vec::with_capacity(update.credits.len());
                for (key, amount) in update.credits.drain(..) {
                    let tracked = changed_keys.contains(&key)
                        || match node.balance_of(&key).await {
                            Ok(v) => v.is_some(),
                            Err(e) => {
                                critical(&format!("verified read during credit filtering failed: {e}"));
                                return;
                            }
                        };
                    if tracked {
                        kept.push((key, amount));
                    }
                }
                update.credits = kept;
            }

            if let Err(e) = node.apply_block(&update).await {
                critical(&format!(
                    "apply_block({n}) failed: {e} — serving stays at block {last}; re-bootstrap required"
                ));
                return;
            }
            last = n;

            if cfg.reconcile_every > 0 && n % cfg.reconcile_every == 0 {
                if !reconcile(&confirm, &node, n, &changed, &credited, cfg.complete, cfg.reconcile_samples).await {
                    return; // reconcile() already logged CRITICAL
                }
            }
        }

        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// Diff up to `samples` of block `n`'s own touched accounts against the
/// independent provider at height `n`. Fetch failures skip the sample
/// (warn); a value mismatch is CRITICAL and returns `false` (stop
/// following, keep serving the last good block).
async fn reconcile(
    confirm: &RpcClient,
    node: &Arc<NodeState>,
    n: u64,
    changed: &[([u8; 20], u128)],
    credited: &[([u8; 20], u128)],
    complete: bool,
    samples: usize,
) -> bool {
    // Tx-changed accounts are exact in both modes; credited recipients
    // are only guaranteed tracked in complete mode.
    let mut candidates: Vec<[u8; 20]> = changed.iter().map(|(a, _)| *a).collect();
    if complete {
        candidates.extend(credited.iter().map(|(a, _)| *a));
    }
    candidates.dedup();

    let mut checked = 0usize;
    for addr in candidates {
        if checked >= samples {
            break;
        }
        let reference = match confirm.balance_at(&addr, n).await {
            Ok(b) => b,
            Err(e) => {
                eprintln!(
                    "risepir-rpc mainnet: reconcile: fetch for 0x{} at {n} failed ({e}); skipping sample",
                    hex20(&addr)
                );
                continue;
            }
        };
        let ours = match node.balance_of(&keccak256(&addr)).await {
            Ok(v) => v.unwrap_or(0),
            Err(e) => {
                critical(&format!("verified read during reconcile failed: {e}"));
                return false;
            }
        };
        if ours != reference {
            critical(&format!(
                "reconcile MISMATCH at block {n} for 0x{}: store says {ours} wei, independent provider says {reference} wei — \
                 the feed has drifted; serving stays at the last applied block; re-bootstrap required",
                hex20(&addr)
            ));
            return false;
        }
        checked += 1;
    }
    if checked > 0 {
        eprintln!("risepir-rpc mainnet: reconcile at block {n}: {checked} account(s) exact vs independent provider");
    }
    true
}

fn critical(msg: &str) {
    eprintln!("risepir-rpc mainnet: CRITICAL: {msg}");
}

fn hex20(bytes: &[u8; 20]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
