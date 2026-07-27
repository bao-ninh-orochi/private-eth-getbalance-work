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
//! - The reconcile check's own health is now observable rather than
//!   inferred (ADR-0027): every checkpoint is classified as empty (no
//!   candidate accounts — nothing to check), successful (≥1 comparison
//!   completed), or **dark** (≥1 attempted, all failed) and recorded into
//!   [`risepir_http::NodeState`]'s [`risepir_http::ReconcileHealth`], which
//!   `GET /healthz` reports. A prolonged dark streak escalates to a
//!   `CRITICAL` log (after `DARK_ESCALATION_THRESHOLD` consecutive dark
//!   checkpoints — this module's own private constant) but **never halts** —
//!   only a value mismatch does that. Halting because a *third-party*
//!   reference provider is unreachable would convert someone else's outage
//!   into this deployment's outage while preventing exactly zero wrong
//!   answers (the feed — and therefore what gets served — is untouched by
//!   whether the reconcile provider answers).

use std::collections::HashSet;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use ikpir_common::SimpleConfig;
use risepir_feed::rpc::{FetchedBlock, RpcClient, RpcFeed};
use risepir_feed::snapshot;
use risepir_http::{NodeState, PirHttpClient};
use risepir_proto::{keccak256, Backend, BlockDelta, Geometry, ValueCodec};
use risepir_server::{DeltaRing, RisePirServer};
use segmented_cuckoo::Segmented3aryCuckooKVStore;

use crate::autosave::StateSaver;
use crate::journal::{self, JournalWriter, ScanStop};
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

/// After how many **consecutive dark** reconcile checkpoints (`reconcile`
/// attempted ≥1 comparison and every attempt failed) to escalate to a
/// `CRITICAL` log line — without halting the follow loop; see the module
/// docs and ADR-0027 for why only a value mismatch halts.
///
/// Chosen from the actual cadence, not a round number picked in the air:
/// at the default `reconcile_every` (30 blocks) and mainnet's ~12 s block
/// time, one checkpoint runs every 30 × 12 s = 360 s = **6 min** of chain
/// time. `20` consecutive dark checkpoints is therefore 20 × 6 min =
/// **120 min (~2 h)** — comfortably past a single transient hiccup at the
/// reference provider, and matching the ~2-hour completely-dark catch-up
/// this repo actually hit on 2026-07-26 (`docs/deploy.md` §5.3), which is
/// the incident this constant exists to make loud the *next* time it
/// happens instead of the operator finding out afterward from the log.
const DARK_ESCALATION_THRESHOLD: u64 = 20;

/// How often the follow loop logs a patch-time summary, in blocks — never
/// per block, which at this deployment's ~12 s block time would be a log
/// line every 12 s forever. 300 blocks is roughly one hour at that
/// cadence: coarse enough that `~/server-complete.log` does not fill up
/// over a multi-day run, fine enough to see within-day drift in the one
/// number `docs/numbers.md` §7 says nobody has measured yet — per-block
/// patch time at the complete mainnet set.
const PATCH_STATS_LOG_INTERVAL_BLOCKS: u64 = 300;

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
    /// Feed endpoints, in priority order — each must serve
    /// `debug_traceBlockByNumber` + `prestateTracer`. dRPC's keyless
    /// endpoint does.
    ///
    /// More than one is strongly recommended for a long-running
    /// deployment: public providers refuse *individual* heavy blocks on
    /// plan limits, deterministically, and since the follow loop may
    /// never skip a block it would otherwise wedge there forever
    /// (`RpcFeed::new_multi`). Later entries are consulted only when
    /// earlier ones fail, so a rate-limited endpoint is a fine fallback.
    pub feed_urls: Vec<String>,
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
    /// snapshot bootstrap, every [`Self::save_interval_secs`] while
    /// following (ADR-0025), and on Ctrl-C.
    pub state: Option<PathBuf>,
    /// Seconds between periodic state saves (`--save-interval`), measured
    /// from the previous save's completion; `0` disables the periodic
    /// trigger (bootstrap and Ctrl-C saves still happen). Only meaningful
    /// with [`Self::state`]. See ADR-0025 for why the save runs inside
    /// the follow loop rather than on its own timer task.
    pub save_interval_secs: u64,
    /// `--journal-restore` (default `false`): when a `--state` file and
    /// its `.journal` sidecar both exist and the journal's header matches
    /// the file's digest, replay the journal onto the loaded state before
    /// serving starts, resuming above the base file's own height instead
    /// of at it. Off by default: with it off, the journal is only
    /// *scanned* (report-only — `docs/adr/README.md` ADR-0026) so an
    /// operator can watch it accumulate real evidence before trusting it.
    pub journal_restore: bool,
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
    /// Serve the browser front end (ADR-0019) from this directory, on the
    /// PIR port's own origin. `None` leaves the deployment headless — the
    /// PIR transport and `cast`/MetaMask still work exactly as before.
    pub web_dir: Option<PathBuf>,
}

impl Default for MainnetConfig {
    fn default() -> Self {
        Self {
            bind: Ipv4Addr::LOCALHOST,
            rpc_port: 8545,
            pir_port: 8645,
            // Ordered: dRPC serves nearly every block in ~1 s but refuses
            // occasional heavy ones on its free plan; merkle.io serves
            // those (it rate-limits under sustained load, which does not
            // matter for the ~1-in-600 blocks it is actually asked for).
            feed_urls: vec![
                "https://eth.drpc.org".to_string(),
                "https://eth.merkle.io".to_string(),
            ],
            confirm_url: "https://ethereum-rpc.publicnode.com".to_string(),
            snapshot: Vec::new(),
            snapshot_block: None,
            snapshot_accounts: None,
            state: None,
            // 30 min: worst-case on-disk staleness ≈ interval + one save
            // duration, so a kill -9 costs minutes of replay instead of
            // hours (ADR-0025 has the arithmetic and the disk-write cost).
            save_interval_secs: 1800,
            journal_restore: false,
            partial: false,
            partial_capacity: 4_000_000,
            proxy_upstream: None,
            reconcile_every: 30,
            reconcile_samples: 8,
            ring_capacity: 600,
            lwe_dim: None,
            web_dir: None,
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
    /// The state saver, when `--state` was given — `main`'s Ctrl-C path
    /// saves through it so the shutdown save serializes with any
    /// in-flight autosave (two concurrent writers to `<path>.tmp` would
    /// interleave into garbage and rename it over the good file).
    pub saver: Option<Arc<StateSaver>>,
    /// Whether the browser front end is being served (ADR-0019).
    pub web_served: bool,
}

/// Fatal deployment-configuration error: print and exit. Everything this
/// wraps happens before serving starts, so exiting is the honest move —
/// there is no traffic to keep alive.
fn die(msg: impl std::fmt::Display) -> ! {
    eprintln!("risepir-rpc mainnet: fatal: {msg}");
    std::process::exit(1);
}

/// What the bootstrap arm ("state file > snapshot > partial") produces,
/// beyond the server itself — the extra plumbing state-save/journal
/// wiring needs (ADR-0025 / ADR-0026).
struct Bootstrap {
    /// The reassembled or freshly built server.
    server: state::Server,
    /// Whether it serves the complete nonzero-balance set.
    complete: bool,
    /// Height actually persisted at `cfg.state`'s path right now, if any
    /// — seeds [`StateSaver`]'s skip-if-unchanged check (ADR-0025).
    /// Distinct from the in-memory server's own height whenever
    /// `--journal-restore` replayed the file forward without yet
    /// re-saving it: the file on disk still holds the *base* height.
    on_disk_height: Option<u64>,
    /// The journal appender to hand [`StateSaver::new`], if one is
    /// already safe to resume this run (ADR-0026) — an adopted restart,
    /// or a fresh one created right after a snapshot bootstrap's first
    /// save. `None` is entirely normal (e.g. partial bootstrap, or a
    /// restart with `--journal-restore` off and the journal ahead of the
    /// base) — the next successful save's rotation starts one.
    initial_journal: Option<JournalWriter>,
    /// Deltas a `--journal-restore` replay produced, to seed the fresh
    /// [`NodeState`]'s delta index with ([`NodeState::seed_history`]) —
    /// empty whenever nothing was replayed.
    tail_deltas: Vec<BlockDelta>,
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
    let feed = match RpcFeed::new_multi(cfg.feed_urls.clone(), 1).await {
        Ok(f) => f,
        Err(e) => die(format!("feed {}: {e}", cfg.feed_urls.join(", "))),
    };
    eprintln!(
        "risepir-rpc mainnet: feed endpoints (in order): {}",
        feed.urls().join(" -> ")
    );

    // ── Bootstrap: state file > snapshot > partial ─────────────────────
    let bootstrap: Bootstrap = if let Some(path) = cfg.state.as_ref().filter(|p| p.exists()) {
        if !cfg.snapshot.is_empty() {
            eprintln!(
                "risepir-rpc mainnet: note: state file {} exists; --snapshot is ignored \
                 (delete the state file to re-bootstrap from the snapshot)",
                path.display()
            );
        }
        let journal_path = journal::journal_path_for(path);

        if cfg.journal_restore {
            // ── --journal-restore ON: replay onto raw parts before the
            // store is built, so the fresh server starts at the
            // journal's height instead of the base file's own. ──
            eprintln!("risepir-rpc mainnet: loading state (--journal-restore) from {} ...", path.display());
            let started = std::time::Instant::now();
            let restored = state::load_with_journal_restore(path, backend_config.clone(), &codec, cfg.ring_capacity)
                .unwrap_or_else(|e| die(format!("loading {} with journal restore: {e}", path.display())));
            let state::RestoredState {
                loaded: state::LoadedState { server, complete, .. },
                replayed,
                base_block,
                tail_deltas,
                adopt_at,
                scan_stop,
            } = restored;
            let plaintext_bits = server.params().plaintext_bits;

            eprintln!(
                "risepir-rpc mainnet: state loaded in {:.1}s — block {}, {} accounts, {}",
                started.elapsed().as_secs_f64(),
                server.block(),
                server.num_items(),
                if complete { "complete set" } else { "PARTIAL set" },
            );
            if let Some(ScanStop::Invalid { offset, reason }) = &scan_stop {
                eprintln!(
                    "risepir-rpc mainnet: WARNING: journal replay stopped at byte {offset} ({reason}) — \
                     the follow loop will fetch the remainder over the network"
                );
            }
            if replayed > 0 {
                eprintln!(
                    "risepir-rpc mainnet: journal replayed: {replayed} block(s) in {:.1}s — resuming at block {} (base was {base_block})",
                    started.elapsed().as_secs_f64(),
                    server.block(),
                );
            } else if adopt_at.is_some() {
                eprintln!("risepir-rpc mainnet: journal matched the base but had nothing new to replay");
            } else {
                eprintln!("risepir-rpc mainnet: no usable journal found; serving from the base state file alone");
            }

            let initial_journal = adopt_at.and_then(|(end_offset, end_height)| {
                match JournalWriter::adopt(&journal_path, plaintext_bits, end_offset, end_height) {
                    Ok(w) => Some(w),
                    Err(e) => {
                        eprintln!("risepir-rpc mainnet: WARNING: could not adopt the journal for continued appending: {e}");
                        None
                    }
                }
            });

            Bootstrap {
                server,
                complete,
                on_disk_height: Some(base_block),
                initial_journal,
                tail_deltas,
            }
        } else {
            // ── --journal-restore OFF (default): load the base exactly
            // as before, but scan the journal read-only and report what
            // it would have done — the soak signal (ADR-0026). ──
            eprintln!("risepir-rpc mainnet: loading state from {} ...", path.display());
            let started = std::time::Instant::now();
            let state::LoadedState { server, complete, digest } = state::load(path, backend_config.clone(), &codec)
                .unwrap_or_else(|e| die(format!("loading {}: {e}", path.display())));
            eprintln!(
                "risepir-rpc mainnet: state loaded in {:.1}s — block {}, {} accounts, {}",
                started.elapsed().as_secs_f64(),
                server.block(),
                server.num_items(),
                if complete { "complete set" } else { "PARTIAL set" },
            );

            let plaintext_bits = server.params().plaintext_bits;
            let arity = server.params().arity() as u32;
            let b = server.block();
            let initial_journal = match digest {
                None => {
                    if journal_path.exists() {
                        eprintln!(
                            "risepir-rpc mainnet: journal present but this is a legacy RPST1 state file \
                             (no digest to bind it to) — ignoring it until the next save upgrades the state file"
                        );
                    }
                    None
                }
                Some(digest) => match journal::scan_report_only(&journal_path, digest, plaintext_bits, arity) {
                    Ok(Some(report)) => {
                        match &report.stop {
                            ScanStop::Eof => eprintln!(
                                "risepir-rpc mainnet: journal intact: {} records to block {} (--journal-restore to use)",
                                report.count, report.end_height
                            ),
                            ScanStop::Invalid { offset, reason } => eprintln!(
                                "risepir-rpc mainnet: journal corrupt at byte {offset} ({reason}); usable prefix: \
                                 {} records to block {} (--journal-restore to use)",
                                report.count, report.end_height
                            ),
                        }
                        // Adopt ONLY if the journal exactly matches this
                        // base and ends at height B — i.e. it was never
                        // ahead. Ahead means appending now would gap
                        // (the next real append is B+1, which the
                        // journal already has a record for); leave that
                        // file untouched (it is someone's recovery data)
                        // and let the next save's rotation start fresh.
                        if report.end_height == b {
                            match JournalWriter::adopt(&journal_path, plaintext_bits, report.end_offset, report.end_height) {
                                Ok(w) => Some(w),
                                Err(e) => {
                                    eprintln!("risepir-rpc mainnet: WARNING: could not adopt journal: {e}");
                                    None
                                }
                            }
                        } else {
                            eprintln!(
                                "risepir-rpc mainnet: journal is ahead of the loaded base (block {b}) — leaving it \
                                 untouched; journaling starts fresh at the next save"
                            );
                            None
                        }
                    }
                    Ok(None) => {
                        // Absent is the normal quiet case; present-but-
                        // unusable (corrupt header, or bound to a
                        // different save — e.g. a crash landed between a
                        // base save and its journal rotation) deserves a
                        // line: silently ignoring a file the operator may
                        // be counting on is how soak evidence gets lost.
                        if journal_path.exists() {
                            eprintln!(
                                "risepir-rpc mainnet: journal present but unusable for this base (corrupt \
                                 header, or bound to a different save) — ignoring it; the next save starts \
                                 a fresh one"
                            );
                        }
                        None
                    }
                    Err(e) => {
                        eprintln!("risepir-rpc mainnet: WARNING: could not scan journal {}: {e}", journal_path.display());
                        None
                    }
                },
            };

            Bootstrap {
                server,
                complete,
                on_disk_height: Some(b),
                initial_journal,
                tail_deltas: Vec::new(),
            }
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
            if ingested.is_multiple_of(5_000_000) {
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

        let mut on_disk_height = None;
        let mut initial_journal = None;
        if let Some(path) = &cfg.state {
            eprintln!("risepir-rpc mainnet: saving state to {} ...", path.display());
            let started = std::time::Instant::now();
            match state::save(&server, &codec, true, path) {
                Ok(state::SaveReport { bytes, digest }) => {
                    eprintln!(
                        "risepir-rpc mainnet: state saved: block {snapshot_block}, {:.2} GB in {:.1}s",
                        bytes as f64 / 1e9,
                        started.elapsed().as_secs_f64(),
                    );
                    on_disk_height = Some(snapshot_block);
                    // The very first journal for this deployment: bound
                    // to the save that just landed, so the follow loop's
                    // next append (snapshot_block + 1) is contiguous
                    // from the start.
                    let journal_path = journal::journal_path_for(path);
                    match JournalWriter::create(&journal_path, digest, snapshot_block, server.params().plaintext_bits) {
                        Ok(w) => initial_journal = Some(w),
                        Err(e) => eprintln!("risepir-rpc mainnet: WARNING: could not create the initial journal: {e}"),
                    }
                }
                // Non-fatal: the server is correct in memory; only restart
                // speed is lost. Say so and continue.
                Err(e) => eprintln!("risepir-rpc mainnet: WARNING: state save failed ({e}); continuing without"),
            }
        }
        Bootstrap {
            server,
            complete: true,
            on_disk_height,
            initial_journal,
            tail_deltas: Vec::new(),
        }
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
        Bootstrap {
            server: RisePirServer::new(store, backend_config.clone(), codec, fin),
            complete: false,
            on_disk_height: None,
            initial_journal: None,
            tail_deltas: Vec::new(),
        }
    } else {
        die("need a data source: --snapshot <csv[.gz]> --snapshot-block <N> (complete), or --state <file> (restart), or --partial (demo)");
    };

    let Bootstrap {
        server,
        complete,
        on_disk_height,
        initial_journal,
        tail_deltas,
    } = bootstrap;
    let head_at_start = server.block();
    let plaintext_bits = server.params().plaintext_bits;
    let node = Arc::new(NodeState::new(server, DeltaRing::new(cfg.ring_capacity), complete));
    if !tail_deltas.is_empty() {
        // Restore-mode only (module docs, `NodeState::seed_history`):
        // these are exactly the deltas just replayed into this same
        // head, so `GET /delta`/`GET /sync` cover them immediately
        // instead of waiting for the follow loop to re-derive that
        // history one live block at a time.
        node.seed_history(tail_deltas).await;
    }

    // ── State autosave (ADR-0025) + delta journal (ADR-0026) ───────────
    // One saver per `--state` path, shared between the follow loop (the
    // periodic trigger) and main's Ctrl-C path (the final save) so the
    // two can never write `<path>.tmp` concurrently; it also owns the
    // current journal appender end to end (rotation on every successful
    // save, appends from the follow loop).
    let saver = cfg.state.as_ref().map(|path| {
        Arc::new(StateSaver::new(
            path.clone(),
            codec,
            complete,
            Duration::from_secs(cfg.save_interval_secs),
            on_disk_height,
            plaintext_bits,
            initial_journal,
        ))
    });
    if let Some(path) = &cfg.state {
        // Whatever a state-file-exists restart already reported above
        // (journal intact / corrupt / ahead / nothing to report) stands;
        // this is the one summary line every bootstrap path prints.
        eprintln!(
            "risepir-rpc mainnet: journal: writing to {} (--journal-restore {})",
            journal::journal_path_for(path).display(),
            if cfg.journal_restore { "ON" } else { "OFF" }
        );
    }

    // Set before anything starts serving (see the method's docs): a probe
    // must never observe a transient "not configured" for a deployment
    // that actually does reconcile.
    node.set_reconcile_configured(cfg.reconcile_every > 0);
    if cfg.reconcile_every > 0 {
        eprintln!(
            "risepir-rpc mainnet: reconcile: every {} block(s), {} sample(s) per checkpoint against {} — \
             GET /healthz reports this check's own health (reconcile_* fields, ADR-0027)",
            cfg.reconcile_every, cfg.reconcile_samples, cfg.confirm_url
        );
    } else {
        eprintln!(
            "risepir-rpc mainnet: reconcile: DISABLED (--reconcile-every 0) — no cross-provider integrity \
             check will run; GET /healthz reports reconcile_configured=0"
        );
    }

    // ── PIR HTTP transport ─────────────────────────────────────────────
    // Loaded before binding anything: a missing or unreadable asset is a
    // startup failure, not a 404 the first visitor discovers.
    let web_assets = match &cfg.web_dir {
        Some(dir) => match risepir_http::WebAssets::load(dir) {
            Ok(assets) => Some(assets),
            Err(e) => die(format!("--web {}: {e}", dir.display())),
        },
        None => None,
    };
    let web_served = web_assets.is_some();

    let pir_listener = tokio::net::TcpListener::bind((cfg.bind, cfg.pir_port))
        .await
        .unwrap_or_else(|e| die(format!("bind PIR port {}: {e}", cfg.pir_port)));
    let pir_addr = pir_listener.local_addr().expect("PIR local_addr");
    tokio::spawn({
        let router = NodeState::router_with_web(node.clone(), web_assets);
        async move {
            if let Err(e) = axum::serve(pir_listener, router).await {
                // A dead listener with a live process is a silent outage;
                // die loudly and let the supervisor restart the unit.
                eprintln!("risepir-rpc mainnet: fatal: PIR listener crashed: {e}");
                std::process::exit(1);
            }
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
            saver: saver.clone(),
        },
    ));

    // ── JSON-RPC front end (one in-process rewind client, ADR-0010) ───
    // Loopback even when bound to 0.0.0.0 — see demo.rs's identical note.
    let pir_client = PirHttpClient::new(crate::front::local_url(cfg.bind, pir_addr.port()));
    let setup_bundle = pir_client
        .setup()
        .await
        .unwrap_or_else(|e| die(format!("GET /setup from our own PIR transport: {e}")));

    let private_eth = Arc::new(PrivateEth::from_setup(
        pir_client,
        setup_bundle,
        codec,
        complete,
        1,
        cfg.proxy_upstream.clone(),
    ));

    let rpc_listener = tokio::net::TcpListener::bind((cfg.bind, cfg.rpc_port))
        .await
        .unwrap_or_else(|e| die(format!("bind JSON-RPC port {}: {e}", cfg.rpc_port)));
    let rpc_addr = rpc_listener.local_addr().expect("RPC local_addr");
    tokio::spawn({
        let router = crate::rpc::router(private_eth);
        async move {
            if let Err(e) = axum::serve(rpc_listener, router).await {
                eprintln!("risepir-rpc mainnet: fatal: JSON-RPC listener crashed: {e}");
                std::process::exit(1);
            }
        }
    });

    MainnetHandle {
        web_served,
        rpc_addr,
        pir_addr,
        complete,
        head_at_start,
        node,
        saver,
    }
}

struct FollowConfig {
    complete: bool,
    reconcile_every: u64,
    reconcile_samples: usize,
    start_at: u64,
    saver: Option<Arc<StateSaver>>,
}

/// Pure aggregation of [`NodeState::apply_block`]'s measured hint-patch
/// duration — see that method's docs for exactly what it measures (lock-
/// *wait* time is deliberately excluded) — plus the same block's mutation
/// count, over a window of blocks. Free of `RpcFeed`/any network type, so
/// the follow loop's summary-logging arithmetic is unit-testable without a
/// live feed.
#[derive(Clone, Copy, Debug, Default)]
struct PatchStats {
    count: u64,
    total: Duration,
    min: Option<Duration>,
    max: Option<Duration>,
    /// Sum of `K` (mutations/block — `update.changes.len() +
    /// update.credits.len()`, the same "mutations/block" quantity
    /// `crates/xtask/src/bench.rs` sweeps over) across every block folded
    /// in. Carried alongside the duration because patch time is a
    /// function of `K` (`docs/numbers.md` §2) — a mean patch time without
    /// it is uninterpretable.
    total_k: u64,
}

impl PatchStats {
    /// Folds in one block's measured patch duration and mutation count.
    fn record(&mut self, duration: Duration, k: usize) {
        self.count += 1;
        self.total += duration;
        self.min = Some(self.min.map_or(duration, |m| m.min(duration)));
        self.max = Some(self.max.map_or(duration, |m| m.max(duration)));
        self.total_k += k as u64;
    }

    /// Mean patch time, in milliseconds, over every block folded in so far
    /// — `0.0` on an empty window (the follow loop never logs one; kept
    /// total rather than partial regardless, so it stays meaningful on
    /// its own in a test).
    fn mean_ms(&self) -> f64 {
        if self.count == 0 {
            0.0
        } else {
            self.total.as_secs_f64() * 1000.0 / self.count as f64
        }
    }

    /// Mean mutations/block (`K`) over the same window.
    fn mean_k(&self) -> f64 {
        if self.count == 0 {
            0.0
        } else {
            self.total_k as f64 / self.count as f64
        }
    }
}

/// Logs one patch-time summary line, in this crate's `eprintln!` style —
/// see [`PATCH_STATS_LOG_INTERVAL_BLOCKS`] for the cadence. The caller
/// resets the accumulator immediately after.
fn log_patch_stats(through_block: u64, stats: &PatchStats) {
    eprintln!(
        "risepir-rpc mainnet: patch stats over last {count} block(s) (through block {through_block}): \
         mean {mean:.4} ms, min {min:.4} ms, max {max:.4} ms, mean K {mean_k:.1} mutations/block",
        count = stats.count,
        mean = stats.mean_ms(),
        min = stats.min.unwrap_or_default().as_secs_f64() * 1000.0,
        max = stats.max.unwrap_or_default().as_secs_f64() * 1000.0,
        mean_k = stats.mean_k(),
    );
}

/// The forever loop: poll `finalized`, apply each new block exactly once,
/// reconcile on cadence. See the module docs for the failure posture.
///
/// This task is also where periodic state saves run (ADR-0025): it is
/// `NodeState`'s **only** writer, so a save executed here between blocks
/// can never have a writer queued behind its read guard — which is what
/// keeps `/answer` flowing for the whole save (tokio's fair `RwLock`
/// parks new readers behind any queued writer). The trigger sits at the
/// top of both loop bodies so it fires in every phase — caught-up
/// polling, catch-up replay, and the fetch-retry loop a refused block
/// causes (ADR-0024) — and always *after* the previous iteration's
/// reconcile, so a state that just failed reconciliation is never the
/// one being persisted (the loop exits before the next trigger).
async fn follow_loop(feed: RpcFeed, confirm: RpcClient, node: Arc<NodeState>, cfg: FollowConfig) {
    let mut last = cfg.start_at;
    let mut patch_stats = PatchStats::default();
    loop {
        if let Some(saver) = &cfg.saver {
            // Outcome/error ignored on purpose: the saver logs, and a
            // failed save must not stop the follow loop (it costs restart
            // speed, never correctness; the next interval retries).
            let _ = saver.maybe_save(&node).await;
        }

        let finalized = match feed.finalized().await {
            Ok(f) => f,
            Err(e) => {
                eprintln!("risepir-rpc mainnet: follow: finalized poll failed ({e}); retrying");
                tokio::time::sleep(RETRY_INTERVAL).await;
                continue;
            }
        };

        while last < finalized {
            if let Some(saver) = &cfg.saver {
                let _ = saver.maybe_save(&node).await;
            }

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

            let (delta, patch_duration) = match node.apply_block(&update).await {
                Ok(d) => d,
                Err(e) => {
                    critical(&format!(
                        "apply_block({n}) failed: {e} — serving stays at block {last}; re-bootstrap required"
                    ));
                    return;
                }
            };
            last = n;

            // Delta journal (ADR-0026): one append per applied block,
            // outside any lock this loop holds (it holds none here).
            if let Some(saver) = &cfg.saver {
                let n_items = node.with_server(|s| s.num_items()).await;
                saver.append_delta(&delta, n_items).await;
            }

            // Per-block patch-time instrumentation (docs/numbers.md §7):
            // `K` mirrors the bench harness's own "mutations/block" —
            // every absolute change plus every (already-filtered, above)
            // withdrawal credit actually applied to the store. Accumulate
            // rather than log every block — see
            // `PATCH_STATS_LOG_INTERVAL_BLOCKS`.
            //
            // Deliberately *after* the journal append and outside its
            // `if let`: the measurement is of the hint patch, which
            // happened either way, so it must not become conditional on a
            // journal being configured.
            patch_stats.record(patch_duration, update.changes.len() + update.credits.len());
            if n.is_multiple_of(PATCH_STATS_LOG_INTERVAL_BLOCKS) {
                log_patch_stats(n, &patch_stats);
                patch_stats = PatchStats::default();
            }

            // Feed `GET /recent` (ADR-0019). Only *after* a successful
            // apply: an address is offered to the front end as queryable
            // exactly when the deployment actually holds it, never before.
            // These are the block's own touched addresses — public chain
            // data, and the same list for every caller.
            node.note_recent(changed.iter().map(|(addr, _)| *addr)).await;

            if cfg.reconcile_every > 0 && n.is_multiple_of(cfg.reconcile_every) {
                let outcome = reconcile(
                    &confirm,
                    &node,
                    n,
                    &changed,
                    &credited,
                    cfg.complete,
                    cfg.reconcile_samples,
                    cfg.reconcile_every,
                )
                .await;
                if matches!(outcome, ReconcileOutcome::Halted) {
                    return; // reconcile() already logged CRITICAL and marked the health record halted
                }
            }
        }

        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// One sampled address's comparison outcome against the independent
/// provider — the atom [`classify_checkpoint`] reduces over. A value
/// *mismatch* never becomes a `SampleResult`: [`reconcile`] returns
/// [`ReconcileOutcome::Halted`] the instant it sees one, before the rest of
/// the sample set is even classified — a mismatch is always CRITICAL,
/// independent of how the remaining samples would otherwise classify.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SampleResult {
    /// The independent provider answered and its balance matched ours.
    Matched,
    /// The independent-provider fetch itself failed (timeout, rate limit,
    /// a refused archive-depth read, ...) — not a mismatch, just no answer.
    FetchFailed,
}

/// What one reconciliation checkpoint amounted to, decided purely from its
/// sample outcomes — no network inside this function, which is what makes
/// it unit-testable with no live dependency. This classification is the
/// core of ADR-0027: the old code's `if checked > 0` shortcut could not
/// tell "this block touched nothing worth sampling" apart from "every
/// fetch failed" — both are `checked == 0` — which is exactly the blind
/// spot `docs/deploy.md` records for the 2026-07-26 catch-up (685
/// checkpoints, the independent provider refusing every archive-depth
/// fetch, zero log lines, the old `reconcile` still returning `true`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CheckpointOutcome {
    /// The block had no candidate accounts to sample at all — nothing was
    /// there to check, which is not evidence of anything failing.
    Empty,
    /// At least one comparison completed and every completed comparison
    /// matched (a mismatch would already have short-circuited `reconcile`
    /// before classification ever runs).
    Success {
        /// Comparisons completed this checkpoint.
        checked: usize,
    },
    /// At least one comparison was attempted and *all* of them failed to
    /// fetch from the independent provider.
    Dark {
        /// Fetches attempted (== failed, by construction of this variant).
        attempted: usize,
    },
}

/// Classify a checkpoint from its sample outcomes.
///
/// `candidates` is the number of accounts the block actually touched
/// (before the `samples` cap), passed *separately* from `results.len()` so
/// "no candidates" (an empty block) and "candidates present but none
/// attempted" are never conflated even in principle — see
/// [`CheckpointOutcome::Empty`]'s docs. In practice `results` is only ever
/// empty here because `candidates` was zero: a positive sample cap always
/// attempts at least one candidate when one exists.
fn classify_checkpoint(candidates: usize, results: &[SampleResult]) -> CheckpointOutcome {
    if candidates == 0 {
        return CheckpointOutcome::Empty;
    }
    let checked = results.iter().filter(|r| **r == SampleResult::Matched).count();
    if checked > 0 {
        CheckpointOutcome::Success { checked }
    } else {
        CheckpointOutcome::Dark { attempted: results.len() }
    }
}

/// Whether `consecutive_dark` just crossed a multiple of
/// [`DARK_ESCALATION_THRESHOLD`] — `true` at exactly the threshold, twice
/// it, three times it, ... so a prolonged outage keeps re-paging the
/// operator rather than paging once and going quiet, but no single
/// checkpoint between multiples re-fires the same line (do not spam a log
/// line every ~6 min forever).
fn should_escalate(consecutive_dark: u64) -> bool {
    consecutive_dark > 0 && consecutive_dark.is_multiple_of(DARK_ESCALATION_THRESHOLD)
}

/// What one [`reconcile`] call amounted to. The follow loop only ever
/// stops on `Halted` — everything else, including a fully dark checkpoint,
/// lets following continue; see the module docs and ADR-0027 for why.
enum ReconcileOutcome {
    /// Following continues. The checkpoint's classification has already
    /// been recorded into `node`'s [`risepir_http::ReconcileHealth`] and
    /// logged — including, if applicable, an escalating `CRITICAL` for a
    /// prolonged dark streak.
    Continued,
    /// A value mismatch or a verified-read error. `reconcile` has already
    /// logged `CRITICAL` and marked the health record halted; the follow
    /// loop must stop.
    Halted,
}

/// Diff up to `samples` of block `n`'s own touched accounts against the
/// independent provider at height `n`, classify the checkpoint
/// ([`classify_checkpoint`]), record it into `node`'s reconcile health
/// ([`risepir_http::NodeState::record_reconcile_checkpoint`]), and log it —
/// every checkpoint, including a dark one.
///
/// A fetch failure is not evidence of anything wrong with *this*
/// deployment; it only marks that sample [`SampleResult::FetchFailed`]. A
/// value **mismatch** is CRITICAL and returns [`ReconcileOutcome::Halted`]
/// — stop following, keep serving the last good block — exactly as before
/// this change. Everything else, including a checkpoint that is *entirely*
/// dark, returns [`ReconcileOutcome::Continued`]: see the module docs for
/// why halting on a third party's unavailability is the wrong trade. A
/// prolonged dark streak instead escalates to a `CRITICAL` log at
/// [`DARK_ESCALATION_THRESHOLD`] — loud, but not fatal.
#[allow(clippy::too_many_arguments)]
async fn reconcile(
    confirm: &RpcClient,
    node: &Arc<NodeState>,
    n: u64,
    changed: &[([u8; 20], u128)],
    credited: &[([u8; 20], u128)],
    complete: bool,
    samples: usize,
    reconcile_every: u64,
) -> ReconcileOutcome {
    // Tx-changed accounts are exact in both modes; credited recipients
    // are only guaranteed tracked in complete mode.
    let mut candidates: Vec<[u8; 20]> = changed.iter().map(|(a, _)| *a).collect();
    if complete {
        candidates.extend(credited.iter().map(|(a, _)| *a));
    }
    candidates.dedup();
    let candidate_count = candidates.len();

    let mut checked = 0usize;
    let mut results: Vec<SampleResult> = Vec::new();
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
                results.push(SampleResult::FetchFailed);
                continue;
            }
        };
        let ours = match node.balance_of(&keccak256(&addr)).await {
            Ok(v) => v.unwrap_or(0),
            Err(e) => {
                node.mark_reconcile_halted();
                critical(&format!("verified read during reconcile failed: {e}"));
                return ReconcileOutcome::Halted;
            }
        };
        if ours != reference {
            node.mark_reconcile_halted();
            critical(&format!(
                "reconcile MISMATCH at block {n} for 0x{}: store says {ours} wei, independent provider says {reference} wei — \
                 the feed has drifted; serving stays at the last applied block; re-bootstrap required",
                hex20(&addr)
            ));
            return ReconcileOutcome::Halted;
        }
        results.push(SampleResult::Matched);
        checked += 1;
    }

    let outcome = classify_checkpoint(candidate_count, &results);
    let (record_checked, dark) = match outcome {
        CheckpointOutcome::Empty => (0, false),
        CheckpointOutcome::Success { checked } => (checked, false),
        CheckpointOutcome::Dark { .. } => (0, true),
    };
    let health = node.record_reconcile_checkpoint(n, record_checked, dark);

    match outcome {
        CheckpointOutcome::Empty => {
            eprintln!("risepir-rpc mainnet: reconcile at block {n}: no candidate accounts to check (empty block)");
        }
        CheckpointOutcome::Success { checked } => {
            eprintln!("risepir-rpc mainnet: reconcile at block {n}: {checked} account(s) exact vs independent provider");
        }
        CheckpointOutcome::Dark { attempted } => {
            let since_success = if health.last_success_unix == 0 {
                "no successful comparison yet this run".to_string()
            } else {
                let elapsed = unix_now().saturating_sub(health.last_success_unix);
                format!("{} since the last one, at block {}", format_duration_secs(elapsed), health.last_success_block)
            };
            eprintln!(
                "risepir-rpc mainnet: reconcile: WARNING: block {n}: {attempted} fetch(es) attempted against the \
                 independent provider, {attempted} failed (dark checkpoint #{} in a row); {since_success}",
                health.consecutive_dark
            );
            if should_escalate(health.consecutive_dark) {
                let blocks_dark = health.consecutive_dark.saturating_mul(reconcile_every);
                critical(&format!(
                    "the reconcile integrity backstop has been dark for {} checkpoint(s) (~{blocks_dark} blocks) — \
                     no cross-provider comparison has succeeded since block {} — the independent reconcile provider \
                     appears unavailable; following continues regardless (a third-party outage must not become \
                     this deployment's outage), but the ingest path is running unverified until this clears",
                    health.consecutive_dark, health.last_success_block
                ));
            }
        }
    }

    ReconcileOutcome::Continued
}

/// `Hh`/`Mm`/`Ss`-style duration for log lines — this module's only
/// consumer is the dark-checkpoint warning above.
fn format_duration_secs(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else {
        format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60)
    }
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn critical(msg: &str) {
    eprintln!("risepir-rpc mainnet: CRITICAL: {msg}");
}

fn hex20(bytes: &[u8; 20]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    //! Pure, network-free tests for the reconcile checkpoint's own
    //! classification and escalation logic (ADR-0027) — no `RpcClient`, no
    //! `NodeState`, no tokio runtime: just the decision functions
    //! `reconcile` itself delegates to. This is deliberately the shape
    //! `docs/deploy.md`'s "silently unavailable" incident calls for: the
    //! bug was in the *classification*, and classification is exactly what
    //! is exercised here without needing a live network dependency in
    //! `cargo test`.
    use super::*;

    // ── classify_checkpoint ──────────────────────────────────────────────

    /// The failure mode this whole change exists to catch: every attempted
    /// comparison failed. Must classify as `Dark`, never `Success` — the
    /// old `if checked > 0` gate silently treated this the same as "nothing
    /// to check".
    #[test]
    fn all_fetches_failed_classifies_as_dark_not_success() {
        let results = [SampleResult::FetchFailed, SampleResult::FetchFailed, SampleResult::FetchFailed];
        let outcome = classify_checkpoint(3, &results);
        assert_eq!(outcome, CheckpointOutcome::Dark { attempted: 3 });
    }

    /// A block with no candidate accounts at all (nothing touched worth
    /// sampling) must classify as `Empty`, never `Dark` — an empty block is
    /// not evidence that anything failed.
    #[test]
    fn no_candidates_classifies_as_empty() {
        let outcome = classify_checkpoint(0, &[]);
        assert_eq!(outcome, CheckpointOutcome::Empty);
    }

    /// `Empty` is decided from the candidate count, not from whether any
    /// `results` happen to be present — a checkpoint that genuinely had
    /// zero candidates can never produce a non-empty `results` slice in
    /// practice, but the function must not accidentally key off
    /// `results.is_empty()` instead of `candidates == 0` (that would make
    /// "candidates present but a misconfigured zero sample cap attempted
    /// none" silently read as `Empty` too — still not `Dark`, but for the
    /// wrong reason. Pin the actual contract: candidate count decides.
    #[test]
    fn empty_classification_is_keyed_on_candidate_count() {
        assert_eq!(classify_checkpoint(0, &[SampleResult::Matched]), CheckpointOutcome::Empty);
    }

    /// At least one completed comparison, even amid failures, is `Success`
    /// — a mismatch (which never becomes a `SampleResult`) would already
    /// have short-circuited `reconcile` before classification runs, so any
    /// `Matched` here is trustworthy.
    #[test]
    fn one_match_among_failures_is_success_with_exact_count() {
        let results = [SampleResult::FetchFailed, SampleResult::Matched, SampleResult::FetchFailed, SampleResult::Matched];
        let outcome = classify_checkpoint(4, &results);
        assert_eq!(outcome, CheckpointOutcome::Success { checked: 2 });
    }

    // ── should_escalate ──────────────────────────────────────────────────

    /// No escalation below the threshold, ever.
    #[test]
    fn escalation_does_not_fire_before_threshold() {
        for n in 0..DARK_ESCALATION_THRESHOLD {
            assert!(!should_escalate(n), "must not escalate at consecutive_dark={n}");
        }
    }

    /// Fires at exactly the threshold.
    #[test]
    fn escalation_fires_at_threshold() {
        assert!(should_escalate(DARK_ESCALATION_THRESHOLD));
    }

    /// Re-fires at every further multiple (the operator keeps getting
    /// paged through a prolonged outage) but not on the checkpoints in
    /// between — a single log line at the threshold and then silence
    /// forever would under-alert; one every checkpoint would spam.
    #[test]
    fn escalation_repeats_periodically_not_on_every_checkpoint() {
        assert!(should_escalate(2 * DARK_ESCALATION_THRESHOLD));
        assert!(should_escalate(3 * DARK_ESCALATION_THRESHOLD));
        for offset in 1..DARK_ESCALATION_THRESHOLD {
            assert!(
                !should_escalate(DARK_ESCALATION_THRESHOLD + offset),
                "must not re-fire at consecutive_dark={}",
                DARK_ESCALATION_THRESHOLD + offset
            );
        }
    }

    // `PatchStats` is deliberately free of `RpcFeed`/any network type (see
    // its own docs), so every test here is pure arithmetic — no live feed,
    // no network, no tokio runtime required.

    #[test]
    fn patch_stats_default_is_an_empty_window() {
        let stats = PatchStats::default();
        assert_eq!(stats.count, 0);
        assert_eq!(stats.min, None);
        assert_eq!(stats.max, None);
        assert_eq!(stats.mean_ms(), 0.0);
        assert_eq!(stats.mean_k(), 0.0);
    }

    #[test]
    fn patch_stats_aggregates_count_mean_min_max_and_k() {
        let mut stats = PatchStats::default();
        stats.record(Duration::from_millis(10), 100);
        stats.record(Duration::from_millis(30), 200);
        stats.record(Duration::from_millis(20), 300);

        assert_eq!(stats.count, 3);
        assert_eq!(stats.min, Some(Duration::from_millis(10)));
        assert_eq!(stats.max, Some(Duration::from_millis(30)));
        // mean = (10 + 30 + 20) / 3 = 20 ms
        assert!((stats.mean_ms() - 20.0).abs() < 1e-9, "mean_ms = {}", stats.mean_ms());
        // mean K = (100 + 200 + 300) / 3 = 200
        assert!((stats.mean_k() - 200.0).abs() < 1e-9, "mean_k = {}", stats.mean_k());
    }

    #[test]
    fn patch_stats_single_sample_has_matching_min_max_and_mean() {
        let mut stats = PatchStats::default();
        stats.record(Duration::from_micros(500), 42);

        assert_eq!(stats.count, 1);
        assert_eq!(stats.min, Some(Duration::from_micros(500)));
        assert_eq!(stats.max, Some(Duration::from_micros(500)));
        assert!((stats.mean_ms() - 0.5).abs() < 1e-9);
        assert!((stats.mean_k() - 42.0).abs() < 1e-9);
    }

    #[test]
    fn log_patch_stats_does_not_panic_on_a_populated_or_empty_window() {
        // Nothing here inspects the printed line itself (the arithmetic it
        // prints is exactly what the aggregation tests above already
        // check); this just proves the formatting call is safe to make,
        // including on the `count == 0` case the follow loop never
        // actually reaches but this function should not choke on either.
        let mut stats = PatchStats::default();
        log_patch_stats(0, &stats);
        stats.record(Duration::from_millis(7), 12);
        log_patch_stats(300, &stats);
    }
}
