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
//!   completed), **dark** (≥1 attempted, all failed), or, since ADR-0036,
//!   **deferred** (still more than `RECENT_DEPTH_BLOCKS` behind the
//!   finalized head — no fetch even attempted, because a catch-up replay
//!   is exactly when the independent provider is known to refuse
//!   archive-depth reads). All of this is recorded into
//!   [`risepir_http::NodeState`]'s [`risepir_http::ReconcileHealth`], which
//!   `GET /healthz` reports. A deferred checkpoint counts as dark for
//!   escalation purposes: a prolonged dark-or-deferred streak escalates to
//!   a `CRITICAL` log (after `DARK_ESCALATION_THRESHOLD` consecutive
//!   checkpoints — this module's own private constant) but **never
//!   halts** — only a value mismatch does that. Halting because a
//!   *third-party* reference provider is unreachable would convert someone
//!   else's outage into this deployment's outage while preventing exactly
//!   zero wrong answers (the feed — and therefore what gets served — is
//!   untouched by whether the reconcile provider answers). Every
//!   checkpoint also bounds its own request volume to at most
//!   `samples.saturating_mul(2)` fetch attempts (ADR-0036 §1) rather than
//!   walking the whole candidate list, and every candidate a blind
//!   checkpoint could not verify is queued in a bounded reservoir and
//!   drained a couple at a time once checkpoints resume normally (ADR-0036
//!   §4) — deferring verification rather than skipping it outright.

use std::collections::{HashSet, VecDeque};
use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use ikpir_common::SimpleConfig;
use risepir_feed::rpc::{Address, FetchedBlock, RpcClient, RpcFeed};
use risepir_feed::snapshot;
use risepir_feed::FeedError;
use risepir_http::{NodeState, PirHttpClient, ReconcileHealth};
use risepir_proto::{keccak256, Backend, Balance, BlockDelta, Geometry, ValueCodec};
use risepir_server::{DeltaRing, RisePirServer};
use segmented_cuckoo::Segmented2aryCuckooKVStore;

use crate::autosave::{SaveOutcome, StateSaver};
use crate::hard_refresh::{self, CorrectionQueue};
use crate::journal::{self, JournalWriter, ScanStop};
use crate::private_eth::PrivateEth;
use crate::snapshot_audit::{self, ReservoirSampler};
use crate::snapshot_rewind;
use crate::state;

/// SCF geometry for the mainnet deployment — arity 2, `bucket_size` 4,
/// 32-bit fp, `key_tag(32) ‖ balance(96) ‖ checksum(16)` (ADR-0034; the
/// measured numbers table this used to match, `docs/numbers.md`, was arity
/// 3 before that retune).
const ARITY: u32 = 2;
/// `pub(crate)` for `crate::state`'s load-time geometry guard (ADR-0042),
/// which validates a state file's stored geometry against the one *this
/// binary* would build — and this is that geometry. The guard shares these
/// constants rather than keeping its own copy, because a guard checking
/// against a private mirror would happily pass a state file the bootstrap
/// path would never have written. (`ARITY` stays private: `state.rs` pins
/// arity separately against the compiled store *type*, via `STORE_ARITY`.)
pub(crate) const BUCKET_SIZE: u32 = 4;
/// See [`BUCKET_SIZE`] for why this is `pub(crate)`.
pub(crate) const FINGERPRINT_BITS: u32 = 32;

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
///
/// That `~2 h` is **chain** time, and only equals wall-clock while the
/// deployment is following the head. A catch-up replay applies blocks far
/// faster than 12 s apart (~1 s/block, measured), so the same 20
/// checkpoints arrive in roughly a tenth of the wall-clock time. That is
/// intended — the 2026-07-26 incident this constant is calibrated against
/// *was* a catch-up — but it means an operator watching a re-bootstrap sees
/// the first escalation within minutes, not hours. [`maybe_escalate`] is
/// what keeps that legible, by naming catch-up lag as the cause instead of
/// blaming the reference provider.
const DARK_ESCALATION_THRESHOLD: u64 = 20;

/// How far behind `finalized` the block currently being applied may be
/// before a reconcile checkpoint **defers** instead of attempting any
/// fetch (ADR-0036 §3): during a catch-up replay the independent provider
/// (publicnode's keyless tier) refuses every archive-depth read anyway —
/// measured on the live box on 2026-07-28: 154,010 log lines and ~130,000
/// useless HTTP requests over 432 checkpoints, every single one dark.
/// `64` sits comfortably above a normal live-following lag (a healthy
/// follow loop is within a handful of blocks of `finalized`) and
/// comfortably below the depth publicnode has ever been observed to
/// serve, so a deployment that is merely a little behind still reconciles
/// normally — only a genuine catch-up replay defers.
const RECENT_DEPTH_BLOCKS: u64 = 64;

/// Capacity of the deferred-reservoir backfill queue (ADR-0036 §4): the
/// bounded memory of addresses seen as reconcile candidates during a blind
/// (dark or deferred) checkpoint, verified later once checkpoints run
/// normally again. Sized to comfortably outlast a single
/// [`DARK_ESCALATION_THRESHOLD`]-checkpoint dark streak without needing to
/// remember *everything* a multi-hour catch-up touches — that was never
/// the goal; the reservoir is a bounded best-effort backfill of the
/// safety net, not a second complete audit log.
const DEFERRED_RESERVOIR_CAP: usize = 256;

/// How many deferred-reservoir addresses a single (non-deferred)
/// checkpoint drains, on top of its own normal candidates. Deliberately
/// small: draining is a steady background trickle across many checkpoints
/// once the provider is reachable again, not a burst that would recreate
/// the very request storm ADR-0036 exists to stop.
const RESERVOIR_DRAIN_PER_CHECKPOINT: usize = 2;

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
    ///
    /// The *default* (when no explicit `--save-interval` is given) is
    /// coupled to [`Self::journal_restore`] (ADR-0037): **21600** (6 h)
    /// when restore is on, since the journal then bounds how much a
    /// crash costs to replay and the full save's only remaining job is
    /// capping journal length; **1800** (30 min, ADR-0025's original
    /// value) when restore is off, since the full save is then the only
    /// thing bounding replay. An explicit `--save-interval` always wins
    /// over either default — see [`Self::save_interval_explicit`].
    pub save_interval_secs: u64,
    /// Whether [`Self::save_interval_secs`] came from an explicit
    /// `--save-interval` on the command line, as opposed to being
    /// defaulted from [`Self::journal_restore`] (ADR-0037). Purely
    /// informational — lets the startup summary (`main.rs`) print
    /// "you asked for this" versus "this followed from your
    /// `--journal-restore` setting" — and has no effect on behavior.
    pub save_interval_explicit: bool,
    /// `--journal-restore` / `--no-journal-restore` (default **on**,
    /// ADR-0037 — flips ADR-0026's original opt-in-behind-a-soak default
    /// now that the soak evidence has held up): when a `--state` file and
    /// its `.journal` sidecar both exist and the journal's header matches
    /// the file's digest, replay the journal onto the loaded state before
    /// serving starts, resuming above the base file's own height instead
    /// of at it. `--journal-restore` itself is kept as an accepted bare
    /// flag — a no-op now that it is the default — for scripts that
    /// already pass it and for operators who want to say so explicitly;
    /// `--no-journal-restore` is the new off switch. Off, the journal is
    /// only *scanned* (report-only — `docs/adr/README.md` ADR-0026)
    /// rather than replayed.
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
    /// `--hard-refresh <file>` (ADR-0040): a newline-delimited address
    /// list to quorum-verify against [`Self::refresh_urls`] and correct in
    /// the store wherever every configured provider agrees on a value
    /// that differs from what is currently stored. `None` (the default)
    /// disables the feature entirely — no file is read, no background
    /// task runs. See `crate::hard_refresh` for the whole mechanism.
    pub hard_refresh: Option<PathBuf>,
    /// Independent balance-reference providers `--hard-refresh` and the
    /// post-bootstrap snapshot audit (`Self::snapshot_audit_samples`)
    /// both quorum-check against (ADR-0040) — **every** configured URL
    /// must agree for a value to be trusted; disagreement or a fetch
    /// error is always "skip", never "guess". At least 2 *distinct* URLs
    /// are required whenever either feature actually runs
    /// ([`hard_refresh::validate_refresh_urls`]); the built-in default
    /// pair is two different operators from the ones this deployment
    /// already uses for the feed and for `reconcile`.
    pub refresh_urls: Vec<String>,
    /// `--snapshot-rewind <N>` (ADR-0040): when bootstrapping from
    /// `--snapshot`, treat the snapshot as exact at `snapshot_block - N`
    /// instead of exactly at `snapshot_block`, so the ordinary catch-up
    /// replay re-derives every account the extra `N` blocks touch from
    /// the chain's own absolute post-state — the measured mitigation for
    /// `docs/deploy.md` §2.1's "the snapshot is not exact at its own
    /// boundary" finding. Default **2000**; `0` disables. See
    /// `crate::snapshot_rewind` for what this does and, just as
    /// importantly, does not fix.
    pub snapshot_rewind: u64,
    /// `--snapshot-audit-samples <N>` (ADR-0040): during snapshot ingest,
    /// reservoir-sample this many `(address, ingested-balance)` pairs
    /// (streaming, one pass, never a second read of the shards) and, once
    /// PIR setup finishes, verify each against [`Self::refresh_urls`]'
    /// quorum at `--snapshot-block` — the mechanism that stops the
    /// boundary-error finding from silently reappearing in a *future*
    /// export undetected. Default **512**; `0` disables. Only meaningful
    /// with `--snapshot` (a `--state`/`--partial` bootstrap does not
    /// ingest anything to sample from). See `crate::snapshot_audit`.
    pub snapshot_audit_samples: usize,
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
            // 6 h: with journal_restore on by default (ADR-0037), the
            // journal — not the full save — bounds replay after an
            // ungraceful kill, so the full save only needs to cap journal
            // length and its own write volume. `main.rs`'s parser
            // re-derives this pairing after every flag is read (so it
            // resolves correctly whichever order `--save-interval` and
            // `--journal-restore`/`--no-journal-restore` appear in); this
            // literal only matters to a caller that builds a
            // `MainnetConfig` directly, bypassing the CLI parser.
            save_interval_secs: 21_600,
            save_interval_explicit: false,
            journal_restore: true,
            partial: false,
            partial_capacity: 4_000_000,
            proxy_upstream: None,
            reconcile_every: 30,
            reconcile_samples: 8,
            ring_capacity: 600,
            lwe_dim: None,
            web_dir: None,
            hard_refresh: None,
            // Two different operators from the feed (dRPC/merkle) and the
            // reconcile check (publicnode) — genuine independence, not
            // just a distinct URL string (ADR-0040).
            refresh_urls: vec![
                "https://gateway.tenderly.co/public/mainnet".to_string(),
                "https://eth-mainnet.public.blastapi.io".to_string(),
            ],
            snapshot_rewind: 2_000,
            snapshot_audit_samples: 512,
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
    logln!("risepir-rpc mainnet: fatal: {msg}");
    std::process::exit(1);
}

/// One-line, printed-once-per-startup summary of what the journal has
/// actually bought since the base save (ADR-0037): its own size on disk
/// against the base state file's, so the write-amplification saving this
/// whole change trades on is measurable directly from the log rather than
/// merely asserted in a doc. Called from both the `--journal-restore`
/// ON and OFF bootstrap arms, whenever a base-matching journal was found
/// at all (neither arm calls this for "no usable journal"). Never a
/// per-block log — this fires exactly once, at startup.
///
/// Silently does nothing if either file's size cannot be `stat`ed: by the
/// time either caller reaches this point, both files are already known to
/// exist and have already been read/parsed successfully, so a `stat`
/// failure here would be an extraordinary race (e.g. deleted out from
/// under the process between then and now) — not worth a fatal error over
/// a diagnostic log line.
fn report_journal_savings(state_path: &Path, journal_path: &Path, record_count: u64) {
    let (Ok(state_meta), Ok(journal_meta)) = (
        std::fs::metadata(state_path),
        std::fs::metadata(journal_path),
    ) else {
        return;
    };
    logln!(
        "risepir-rpc mainnet: journal: {record_count} record(s), {} bytes since the base save (base \
         state file is {} bytes) — restoring costs the replay, not the rewrite",
        journal_meta.len(),
        state_meta.len(),
    );
}

/// What a fresh `--snapshot` bootstrap needs later, once `node` exists, to
/// run the post-bootstrap snapshot audit (ADR-0040, `crate::snapshot_audit`)
/// as a background task. `None` from every other bootstrap arm (a
/// `--state`/`--partial` bootstrap does not ingest anything to sample
/// from) and `None` from the snapshot arm too when
/// `--snapshot-audit-samples 0` disabled it or the reservoir happened to
/// sample nothing.
struct PendingAudit {
    /// Up to `--snapshot-audit-samples` `(address, ingested-balance)`
    /// pairs, reservoir-sampled during ingest.
    sample: Vec<([u8; 20], risepir_proto::Balance)>,
    /// The snapshot's own declared exact-at block (`--snapshot-block`) —
    /// the height the audit verifies against, which is deliberately *not*
    /// the same as the server's actual genesis when `--snapshot-rewind`
    /// moved that earlier (the audit is checking the snapshot's own
    /// claim, not whatever replay window the server additionally re-runs).
    snapshot_block: u64,
    /// Total nonzero rows the snapshot ingested — the population size the
    /// audit's measured rate is extrapolated over in the report line.
    total_ingested: u64,
    /// The reservoir sampler's seed (logged at sampling time; carried
    /// through so the persisted sidecar records it too).
    seed: u64,
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
    /// What the post-bootstrap snapshot audit needs to run, if this was a
    /// fresh `--snapshot` bootstrap with sampling enabled — see
    /// [`PendingAudit`].
    pending_audit: Option<PendingAudit>,
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
    logln!(
        "risepir-rpc mainnet: feed endpoints (in order): {}",
        feed.urls().join(" -> ")
    );

    // Claim the --state path for this process's lifetime before anything
    // reads or writes it: refuses a `.journal`-suffixed path (whose
    // sidecar would collide with the state file itself) and holds an
    // advisory lock so a double-started server fails fast here instead
    // of two writers interleaving into the same `<path>.tmp` and
    // destroying the good multi-GB file — 36 GB at the live `(arity 3,
    // bucket_size 4)` deployment today, ≈24 GB once re-bootstrapped to the
    // deployed `(arity 2, bucket_size 4)` geometry (ADR-0034)
    // (`state::acquire_state_path`).
    let _state_lock = cfg.state.as_ref().map(|path| {
        state::acquire_state_path(path)
            .unwrap_or_else(|e| die(format!("--state {}: {e}", path.display())))
    });

    // `--hard-refresh` (ADR-0040) is validated here, before any bootstrap
    // work — a `--snapshot` bootstrap can cost 16+ minutes, and a
    // misconfigured `--refresh-url` is a configuration mistake that should
    // fail in milliseconds, not after paying for an entire ingest + PIR
    // setup first (`crate::snapshot_audit`'s own analogous check, inside
    // the snapshot-ingest arm below, cannot be hoisted here — it needs to
    // know whether this run is actually ingesting a snapshot at all, which
    // is only decided inside that arm).
    if cfg.hard_refresh.is_some() {
        if let Err(e) = hard_refresh::validate_refresh_urls(&cfg.refresh_urls) {
            logln!("risepir-rpc mainnet: fatal: --hard-refresh requires valid --refresh-url config: {e}");
            std::process::exit(2);
        }
    }

    // ── Bootstrap: state file > snapshot > partial ─────────────────────
    let bootstrap: Bootstrap = if let Some(path) = cfg.state.as_ref().filter(|p| p.exists()) {
        if !cfg.snapshot.is_empty() {
            logln!(
                "risepir-rpc mainnet: note: state file {} exists; --snapshot is ignored \
                 (delete the state file to re-bootstrap from the snapshot)",
                path.display()
            );
        }
        let journal_path = journal::journal_path_for(path);

        if cfg.journal_restore {
            // ── --journal-restore ON (default since ADR-0037): replay
            // onto raw parts before the store is built, so the fresh
            // server starts at the journal's height instead of the base
            // file's own. ──
            logln!(
                "risepir-rpc mainnet: loading state (--journal-restore) from {} ...",
                path.display()
            );
            let started = std::time::Instant::now();
            let restored = state::load_with_journal_restore(
                path,
                backend_config.clone(),
                &codec,
                cfg.ring_capacity,
            )
            .unwrap_or_else(|e| {
                die(format!(
                    "loading {} with journal restore: {e}",
                    path.display()
                ))
            });
            let state::RestoredState {
                loaded:
                    state::LoadedState {
                        server, complete, ..
                    },
                replayed,
                base_block,
                tail_deltas,
                adopt_at,
                scan_stop,
                replay_elapsed,
            } = restored;
            let plaintext_bits = server.params().plaintext_bits;

            logln!(
                "risepir-rpc mainnet: state loaded in {:.1}s — block {}, {} accounts, {}",
                started.elapsed().as_secs_f64(),
                server.block(),
                server.num_items(),
                if complete {
                    "complete set"
                } else {
                    "PARTIAL set"
                },
            );
            if let Some(ScanStop::Invalid { offset, reason }) = &scan_stop {
                logln!(
                    "risepir-rpc mainnet: WARNING: journal replay stopped at byte {offset} ({reason}) — \
                     the follow loop will fetch the remainder over the network"
                );
            }
            if replayed > 0 {
                // `replay_elapsed` (ADR-0037), not `started.elapsed()`:
                // the latter also includes the base file's own read,
                // which at the complete mainnet set is the dominant cost
                // and one this feature does not shrink — conflating the
                // two would inflate what "replay" appears to cost.
                logln!(
                    "risepir-rpc mainnet: journal replayed: {replayed} block(s) in {:.3}s — resuming at block {} (base was {base_block})",
                    replay_elapsed.as_secs_f64(),
                    server.block(),
                );
            } else if adopt_at.is_some() {
                logln!(
                    "risepir-rpc mainnet: journal matched the base but had nothing new to replay"
                );
            } else {
                logln!("risepir-rpc mainnet: no usable journal found; serving from the base state file alone");
            }

            let initial_journal = adopt_at.and_then(|(end_offset, end_height)| {
                match JournalWriter::adopt(&journal_path, plaintext_bits, end_offset, end_height) {
                    Ok(w) => Some(w),
                    Err(e) => {
                        logln!("risepir-rpc mainnet: WARNING: could not adopt the journal for continued appending: {e}");
                        None
                    }
                }
            });
            // Measurable, not asserted (ADR-0037): one line, only when a
            // base-matching journal actually existed to measure.
            if adopt_at.is_some() {
                report_journal_savings(path, &journal_path, replayed);
            }

            Bootstrap {
                server,
                complete,
                on_disk_height: Some(base_block),
                initial_journal,
                tail_deltas,
                pending_audit: None,
            }
        } else {
            // ── --journal-restore OFF (--no-journal-restore; ADR-0037
            // flipped the default to ON): load the base exactly as
            // before, but scan the journal read-only and report what it
            // would have done — the original ADR-0026 soak signal, still
            // available for an operator who wants to opt back out. ──
            logln!(
                "risepir-rpc mainnet: loading state from {} ...",
                path.display()
            );
            let started = std::time::Instant::now();
            let state::LoadedState {
                server,
                complete,
                digest,
            } = state::load(path, backend_config.clone(), &codec)
                .unwrap_or_else(|e| die(format!("loading {}: {e}", path.display())));
            logln!(
                "risepir-rpc mainnet: state loaded in {:.1}s — block {}, {} accounts, {}",
                started.elapsed().as_secs_f64(),
                server.block(),
                server.num_items(),
                if complete {
                    "complete set"
                } else {
                    "PARTIAL set"
                },
            );

            let plaintext_bits = server.params().plaintext_bits;
            let arity = server.params().arity() as u32;
            let b = server.block();
            // Every loadable state file carries a digest now — the
            // digest-less case was `RPST1`, which stopped loading at the
            // `xxh3_128` switch — so the old "journal present but this base
            // has no digest to bind it to" arm is gone rather than left
            // unreachable. It could not fire, and if it somehow did its
            // advice ("ignoring it until the next save upgrades the state
            // file") would now be wrong: an RPST1 file never gets a next
            // save, because it is refused at load.
            let initial_journal = match journal::scan_report_only(
                &journal_path,
                digest,
                plaintext_bits,
                arity,
            ) {
                Ok(Some(report)) => {
                    // This whole branch only runs with journal-restore
                    // OFF, which since ADR-0037 means the operator
                    // passed --no-journal-restore explicitly (it is no
                    // longer reachable by omission) — so the remedy is
                    // "drop that flag", not "pass --journal-restore"
                    // (a no-op now that it is the default).
                    match &report.stop {
                        ScanStop::Eof => logln!(
                            "risepir-rpc mainnet: journal intact: {} records to block {} (drop \
                             --no-journal-restore to use it)",
                            report.count, report.end_height
                        ),
                        ScanStop::Invalid { offset, reason } => logln!(
                            "risepir-rpc mainnet: journal corrupt at byte {offset} ({reason}); usable prefix: \
                             {} records to block {} (drop --no-journal-restore to use it)",
                            report.count, report.end_height
                        ),
                    }
                    // Measurable, not asserted (ADR-0037): one line,
                    // regardless of whether this journal is ahead of
                    // or matches the loaded base — either way its
                    // on-disk size against the base file's is real,
                    // reportable evidence.
                    report_journal_savings(path, &journal_path, report.count);
                    // Adopt ONLY if the journal exactly matches this
                    // base and ends at height B — i.e. it was never
                    // ahead. Ahead means appending now would gap
                    // (the next real append is B+1, which the
                    // journal already has a record for); leave that
                    // file untouched (it is someone's recovery data)
                    // and let the next save's rotation start fresh.
                    if report.end_height == b {
                        match JournalWriter::adopt(
                            &journal_path,
                            plaintext_bits,
                            report.end_offset,
                            report.end_height,
                        ) {
                            Ok(w) => Some(w),
                            Err(e) => {
                                logln!(
                                    "risepir-rpc mainnet: WARNING: could not adopt journal: {e}"
                                );
                                None
                            }
                        }
                    } else {
                        logln!(
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
                        logln!(
                            "risepir-rpc mainnet: journal present but unusable for this base (corrupt \
                             header, or bound to a different save) — ignoring it; the next save starts \
                             a fresh one"
                        );
                    }
                    None
                }
                Err(e) => {
                    logln!(
                        "risepir-rpc mainnet: WARNING: could not scan journal {}: {e}",
                        journal_path.display()
                    );
                    None
                }
            };

            Bootstrap {
                server,
                complete,
                on_disk_height: Some(b),
                initial_journal,
                tail_deltas: Vec::new(),
                pending_audit: None,
            }
        }
    } else if !cfg.snapshot.is_empty() {
        let snapshot_block = cfg.snapshot_block.unwrap_or_else(|| {
            die("--snapshot requires --snapshot-block (the block the snapshot is exact at)")
        });

        // The post-bootstrap snapshot audit (ADR-0040) needs at least 2
        // distinct reference providers whenever it will actually sample
        // anything — checked up front, before spending any time on ingest,
        // the same posture `die()` already takes for every other
        // deployment-configuration mistake caught before serving starts.
        if cfg.snapshot_audit_samples > 0 {
            if let Err(e) = hard_refresh::validate_refresh_urls(&cfg.refresh_urls) {
                logln!("risepir-rpc mainnet: fatal: --snapshot-audit-samples requires valid --refresh-url config: {e}");
                std::process::exit(2);
            }
        }

        // --snapshot-rewind (ADR-0040): the genesis the server actually
        // starts at may be earlier than the snapshot's own declared exact
        // block — see `crate::snapshot_rewind`'s docs for what this
        // narrows and, just as importantly, what it does not fix.
        let effective_genesis =
            snapshot_rewind::rewound_genesis(snapshot_block, cfg.snapshot_rewind)
                .unwrap_or_else(|e| die(e));
        if cfg.snapshot_rewind > 0 {
            logln!(
                "risepir-rpc mainnet: --snapshot-rewind {}: treating the snapshot as exact at block {} \
                 instead of the declared {snapshot_block} — the catch-up replay will re-derive every \
                 account the extra {} block(s) touch from the chain's own absolute post-state before \
                 reaching the declared block (~{} extra second(s) of replay at ~1 s/block). This narrows \
                 the boundary error (docs/adr/README.md ADR-0040) but does not close it, and it does not \
                 fix relative withdrawal credits inside the window — --hard-refresh is the remedy for those.",
                cfg.snapshot_rewind, effective_genesis, cfg.snapshot_rewind, cfg.snapshot_rewind,
            );
        }

        let accounts = match cfg.snapshot_accounts {
            Some(n) => n,
            None => {
                logln!("risepir-rpc mainnet: counting snapshot rows (pass --snapshot-accounts to skip) ...");
                match snapshot::count_rows(&cfg.snapshot) {
                    Ok(n) => n,
                    Err(e) => die(e),
                }
            }
        };
        let geom = Geometry::for_accounts(
            accounts.max(1_000),
            ARITY,
            BUCKET_SIZE,
            FINGERPRINT_BITS,
            &codec,
            Backend::Simple,
        )
        .unwrap_or_else(|e| die(format!("geometry for {accounts} accounts: {e}")));
        let sizes = geom.sizes(Backend::Simple, accounts);
        logln!(
            "risepir-rpc mainnet: geometry for {accounts} accounts: {} buckets, server DB {:.2} GB, load {:.3}",
            geom.num_buckets,
            sizes.server_db as f64 / 1e9,
            sizes.load_factor,
        );
        let mut store = Segmented2aryCuckooKVStore::new(
            geom.num_buckets,
            geom.bucket_size,
            geom.fingerprint_bits,
            geom.value_bits,
            geom.plaintext_bits,
        )
        .unwrap_or_else(|e| die(format!("store construction: {e:?}")));

        // Post-bootstrap snapshot audit (ADR-0040): a streaming reservoir
        // over the very rows being ingested, so sampling costs one pass,
        // never a second read of a ~200M-row export. Seeded so the
        // sample is reproducible after the fact from the logged seed;
        // `capacity == 0` (`--snapshot-audit-samples 0`) makes every
        // `observe` call a permanent no-op.
        let audit_seed = snapshot_audit::random_seed();
        let mut reservoir = ReservoirSampler::new(cfg.snapshot_audit_samples, audit_seed);
        if cfg.snapshot_audit_samples > 0 {
            logln!(
                "risepir-rpc mainnet: snapshot audit: reservoir-sampling up to {} address(es) during ingest \
                 (seed={audit_seed})",
                cfg.snapshot_audit_samples
            );
        }

        logln!(
            "risepir-rpc mainnet: ingesting snapshot ({} shard(s)) ...",
            cfg.snapshot.len()
        );
        let started = std::time::Instant::now();
        let mut ingested = 0u64;
        let stats = snapshot::ingest(&cfg.snapshot, |addr20, key, balance| {
            let encoded = codec.encode(&key, balance).map_err(|e| e.to_string())?;
            store.insert(key, &encoded).map_err(|e| format!("{e:?}"))?;
            reservoir.observe(addr20, balance);
            ingested += 1;
            if ingested.is_multiple_of(5_000_000) {
                logln!(
                    "risepir-rpc mainnet:   {ingested} accounts in {:.0}s ...",
                    started.elapsed().as_secs_f64()
                );
            }
            Ok(())
        })
        .unwrap_or_else(|e| die(e));
        logln!(
            "risepir-rpc mainnet: snapshot ingested in {:.0}s — {} rows, {} nonzero, {} zero skipped, max balance {} wei",
            started.elapsed().as_secs_f64(),
            stats.rows,
            stats.nonzero,
            stats.zero_skipped,
            stats.max_balance,
        );

        logln!("risepir-rpc mainnet: running PIR setup (one-time preprocessing) ...");
        let started = std::time::Instant::now();
        // effective_genesis, not snapshot_block: with --snapshot-rewind
        // active the server's *actual* starting height is earlier than
        // the snapshot's declared block, and every log line / persisted
        // height below must reflect what the server truly holds, not the
        // nominal declaration (the audit, further down, is the one place
        // that deliberately keeps using the nominal `snapshot_block`).
        let server = RisePirServer::new(store, backend_config.clone(), codec, effective_genesis);
        logln!(
            "risepir-rpc mainnet: setup done in {:.1}s at block {}",
            started.elapsed().as_secs_f64(),
            server.block(),
        );

        let mut on_disk_height = None;
        let mut initial_journal = None;
        if let Some(path) = &cfg.state {
            logln!(
                "risepir-rpc mainnet: saving state to {} ...",
                path.display()
            );
            let started = std::time::Instant::now();
            match state::save(&server, &codec, true, path) {
                Ok(state::SaveReport { bytes, digest }) => {
                    logln!(
                        "risepir-rpc mainnet: state saved: block {}, {:.2} GB in {:.1}s",
                        server.block(),
                        bytes as f64 / 1e9,
                        started.elapsed().as_secs_f64(),
                    );
                    on_disk_height = Some(server.block());
                    // The very first journal for this deployment: bound
                    // to the save that just landed, so the follow loop's
                    // next append (server.block() + 1) is contiguous
                    // from the start.
                    let journal_path = journal::journal_path_for(path);
                    match JournalWriter::create(&journal_path, digest, server.block(), server.params().plaintext_bits) {
                        Ok(w) => initial_journal = Some(w),
                        Err(e) => logln!("risepir-rpc mainnet: WARNING: could not create the initial journal: {e}"),
                    }
                }
                // Non-fatal: the server is correct in memory; only restart
                // speed is lost. Say so and continue.
                Err(e) => logln!(
                    "risepir-rpc mainnet: WARNING: state save failed ({e}); continuing without"
                ),
            }
        }

        // Post-bootstrap snapshot audit (ADR-0040): hand off the
        // reservoir's finished sample (if any) to be verified once `node`
        // exists, later in `spawn`. Verifies against the snapshot's own
        // *declared* block, not `effective_genesis` — the audit is
        // checking whether the export's claim at its own boundary held
        // up, independent of whatever extra replay window the server
        // additionally re-runs.
        let sample = reservoir.into_sample();
        let pending_audit = if sample.is_empty() {
            None
        } else {
            Some(PendingAudit {
                sample,
                snapshot_block,
                total_ingested: stats.nonzero,
                seed: audit_seed,
            })
        };

        Bootstrap {
            server,
            complete: true,
            on_disk_height,
            initial_journal,
            tail_deltas: Vec::new(),
            pending_audit,
        }
    } else if cfg.partial {
        let fin = match feed.finalized().await {
            Ok(f) => f,
            Err(e) => die(format!("fetching finalized block: {e}")),
        };
        let geom = Geometry::for_accounts(
            cfg.partial_capacity,
            ARITY,
            BUCKET_SIZE,
            FINGERPRINT_BITS,
            &codec,
            Backend::Simple,
        )
        .unwrap_or_else(|e| die(format!("geometry: {e}")));
        let store = Segmented2aryCuckooKVStore::new(
            geom.num_buckets,
            geom.bucket_size,
            geom.fingerprint_bits,
            geom.value_bits,
            geom.plaintext_bits,
        )
        .unwrap_or_else(|e| die(format!("store construction: {e:?}")));
        logln!(
            "risepir-rpc mainnet: PARTIAL bootstrap at finalized block {fin} — empty set, capacity {} accounts.",
            cfg.partial_capacity
        );
        logln!(
            "risepir-rpc mainnet: partial mode serves only accounts touched from here on; everything else ERRORS (never 0x0)."
        );
        Bootstrap {
            server: RisePirServer::new(store, backend_config.clone(), codec, fin),
            complete: false,
            on_disk_height: None,
            initial_journal: None,
            tail_deltas: Vec::new(),
            pending_audit: None,
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
        pending_audit,
    } = bootstrap;
    let head_at_start = server.block();
    let plaintext_bits = server.params().plaintext_bits;
    let node = Arc::new(NodeState::new(
        server,
        DeltaRing::new(cfg.ring_capacity),
        complete,
    ));
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
        logln!(
            "risepir-rpc mainnet: journal: writing to {} (--journal-restore {})",
            journal::journal_path_for(path).display(),
            if cfg.journal_restore { "ON" } else { "OFF" }
        );
    }

    // Set before anything starts serving, same reasoning as the reconcile
    // line right below (ADR-0039): `GET /metrics`'s
    // `risepir_state_save_configured` must never read as "configured" for
    // a deployment that was never given `--state`, nor "not configured"
    // for one that was but simply has not saved yet.
    node.set_state_saving_configured(cfg.state.is_some());

    // ── Hard-refresh (ADR-0040) + snapshot audit (ADR-0040) ─────────────
    // Both run as background tasks, never awaited here: neither may block
    // serving or following (see `crate::hard_refresh`'s and
    // `crate::snapshot_audit`'s module docs for exactly why that is safe
    // — briefly, neither ever touches the PIR server's write lock).

    // A restart that only *loads* a state file (no fresh ingest this run)
    // has nothing new to sample, but the last snapshot audit's finding
    // should not silently vanish from `/healthz` just because the process
    // restarted. Reading this unconditionally on `cfg.state.is_some()` is
    // safe even on a fresh `--snapshot` bootstrap that reuses an old path:
    // if no sidecar exists yet (the ordinary case), this is a no-op, and
    // if a *stale* one from a previous lineage happens to exist, it is
    // overwritten within minutes by the fresh audit spawned below.
    if let Some(path) = &cfg.state {
        if let snapshot_audit::AuditSidecar::Known(record) =
            snapshot_audit::read_sidecar(&snapshot_audit::sidecar_path(path))
        {
            node.set_snapshot_audit_line(snapshot_audit::healthz_value(&record));
        }
    }

    // Always constructed (cheap, and empty by default) so the follow loop
    // has exactly one thing to drain from, whether or not --hard-refresh
    // is configured — see `FollowConfig::corrections`.
    let corrections = Arc::new(CorrectionQueue::new());

    if let Some(path) = &cfg.hard_refresh {
        // --refresh-url was already validated at the very top of `spawn`
        // (before any bootstrap work), so this is wiring, not validation:
        // hard-refresh is not specific to a fresh snapshot ingest — it is
        // equally meaningful against a `--state`-restarted server, which
        // is why this lives here rather than inside any one bootstrap arm.
        logln!(
            "risepir-rpc mainnet: hard-refresh: {} configured; checking will run in the background \
             (never blocks serving or following)",
            path.display()
        );
        tokio::spawn(hard_refresh::run(
            node.clone(),
            corrections.clone(),
            path.clone(),
            cfg.refresh_urls.clone(),
        ));
    }

    if let Some(audit) = pending_audit {
        tokio::spawn(snapshot_audit::verify(
            node.clone(),
            audit.sample,
            audit.snapshot_block,
            cfg.refresh_urls.clone(),
            audit.total_ingested,
            audit.seed,
            cfg.state.clone(),
        ));
    }

    // Set before anything starts serving (see the method's docs): a probe
    // must never observe a transient "not configured" for a deployment
    // that actually does reconcile.
    node.set_reconcile_configured(cfg.reconcile_every > 0);
    if cfg.reconcile_every > 0 {
        logln!(
            "risepir-rpc mainnet: reconcile: every {} block(s), {} sample(s) per checkpoint against {} — \
             GET /healthz reports this check's own health (reconcile_* fields, ADR-0027)",
            cfg.reconcile_every, cfg.reconcile_samples, cfg.confirm_url
        );
    } else {
        logln!(
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
                logln!("risepir-rpc mainnet: fatal: PIR listener crashed: {e}");
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
            corrections: corrections.clone(),
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
                logln!("risepir-rpc mainnet: fatal: JSON-RPC listener crashed: {e}");
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
    /// Hard-refresh corrections (ADR-0040) waiting to ride a block's
    /// `changes` — always a live queue (possibly permanently empty when
    /// `--hard-refresh` was never set), so the follow loop can drain it
    /// unconditionally rather than branching on an `Option` every block.
    corrections: Arc<CorrectionQueue>,
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

/// Logs one patch-time summary line, in this crate's `logln!` style —
/// see [`PATCH_STATS_LOG_INTERVAL_BLOCKS`] for the cadence. The caller
/// resets the accumulator immediately after.
fn log_patch_stats(through_block: u64, stats: &PatchStats) {
    logln!(
        "risepir-rpc mainnet: patch stats over last {count} block(s) (through block {through_block}): \
         mean {mean:.4} ms, min {min:.4} ms, max {max:.4} ms, mean K {mean_k:.1} mutations/block",
        count = stats.count,
        mean = stats.mean_ms(),
        min = stats.min.unwrap_or_default().as_secs_f64() * 1000.0,
        max = stats.max.unwrap_or_default().as_secs_f64() * 1000.0,
        mean_k = stats.mean_k(),
    );
}

/// Runs one [`StateSaver::maybe_save`] tick and publishes the outcome into
/// `node` for `GET /metrics` (ADR-0039, `NodeState::record_save_outcome`/
/// `record_save_failure`) — the follow loop is the only thing that ever
/// calls this, so it is also the only thing that can publish it;
/// `NodeState` holds no reference back to the saver (`risepir-http`
/// cannot depend on `risepir-rpc`, the reverse of this crate's own
/// dependency direction).
///
/// Timing wraps the whole `maybe_save` call rather than reaching inside
/// it: for `Saved`, that *is* the save duration (the interesting case, and
/// the only one this method reports a duration for); for
/// `Unchanged`/`NotDue`/`Busy` it is just a lock check, cheap enough that
/// timing it costs nothing and there is no duration to publish for those
/// anyway. All of `maybe_save`'s own retry/skip/logging semantics are
/// unchanged — this only adds the publish step around it.
async fn record_save_tick(saver: &StateSaver, node: &NodeState) {
    let t0 = std::time::Instant::now();
    let outcome = saver.maybe_save(node).await;
    let elapsed = t0.elapsed();
    match outcome {
        Ok(SaveOutcome::Saved { bytes, .. }) => {
            node.record_save_outcome(unix_now(), elapsed, bytes)
        }
        Ok(SaveOutcome::Unchanged { .. } | SaveOutcome::NotDue | SaveOutcome::Busy) => {}
        Err(_) => node.record_save_failure(),
    }
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
    // Persists across every checkpoint for the life of the loop (ADR-0036
    // §4) — addresses queued here during a blind checkpoint are verified
    // later, a couple at a time, once checkpoints run normally again.
    let mut reservoir = DeferredReservoir::default();
    loop {
        if let Some(saver) = &cfg.saver {
            // Outcome/error ignored by this loop's own control flow (the
            // saver logs, and a failed save must not stop following — it
            // costs restart speed, never correctness; the next interval
            // retries); `record_save_tick` still publishes it for
            // `GET /metrics` (ADR-0039).
            record_save_tick(saver, &node).await;
        }

        let finalized = match feed.finalized().await {
            Ok(f) => f,
            Err(e) => {
                logln!("risepir-rpc mainnet: follow: finalized poll failed ({e}); retrying");
                tokio::time::sleep(RETRY_INTERVAL).await;
                continue;
            }
        };
        // The one glance ADR-0039 exists for: `risepir_finalized_block`
        // and `risepir_block_lag` against `risepir_head_block` on
        // `GET /metrics`, published from the only place that ever learns
        // this value.
        node.set_finalized(finalized);

        while last < finalized {
            if let Some(saver) = &cfg.saver {
                record_save_tick(saver, &node).await;
            }

            let n = last + 1;
            let fetched = match feed.block_update(n).await {
                Ok(f) => f,
                Err(e) => {
                    logln!("risepir-rpc mainnet: follow: block {n} fetch failed ({e}); retrying");
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
                                critical(&format!(
                                    "verified read during credit filtering failed: {e}"
                                ));
                                return;
                            }
                        };
                    if tracked {
                        kept.push((key, amount));
                    }
                }
                update.credits = kept;
            }

            // Hard-refresh corrections (ADR-0040): drained FIFO, capped at
            // MAX_CORRECTIONS_PER_BLOCK. A correction set larger than that
            // cap can sit queued across several blocks, so every drained
            // correction is re-checked against the store's *live* balance
            // (`filter_stale_corrections`) immediately before use and
            // dropped if an ordinary feed-applied block has since moved
            // that account on — never overwrite fresher, correct on-chain
            // data with a stale corrected value. Survivors are placed
            // FIRST so the feed's own changes for *this* block — appended
            // after, and last-entry-for-a-key-wins per
            // `BlockUpdate::changes`'s own documented contract — always
            // take precedence over a correction for the same account.
            // Draining an empty queue (the common case when
            // `--hard-refresh` was never set) is a cheap no-op, so this
            // runs unconditionally every block rather than behind an
            // `Option` check.
            let drained = cfg
                .corrections
                .drain_up_to(hard_refresh::MAX_CORRECTIONS_PER_BLOCK);
            if !drained.is_empty() {
                let (still_valid, stale) =
                    match hard_refresh::filter_stale_corrections(&node, drained).await {
                        Ok(result) => result,
                        Err(e) => {
                            critical(&format!(
                            "verified read while re-checking hard-refresh corrections failed: {e}"
                        ));
                            return;
                        }
                    };
                if stale > 0 {
                    logln!(
                        "risepir-rpc mainnet: hard-refresh: {stale} correction(s) dropped as stale in block {n} \
                         (the account changed since the check; the feed's own value wins)"
                    );
                }
                if !still_valid.is_empty() {
                    let still_queued = cfg.corrections.len();
                    logln!(
                        "risepir-rpc mainnet: hard-refresh: applying {} correction(s) in block {n} \
                         ({still_queued} still queued)",
                        still_valid.len()
                    );
                    update.changes = hard_refresh::prepend_corrections(
                        still_valid,
                        std::mem::take(&mut update.changes),
                    );
                }
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
                // `GET /metrics`'s `risepir_journal_records_since_save` /
                // `risepir_journal_broken` (ADR-0039) — published every
                // block rather than only at rotation, so the gauge tracks
                // between saves too, not just at their boundaries.
                let js = saver.journal_status().await;
                node.record_journal_status(js.records_since_save, js.broken);
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
            node.note_recent(changed.iter().map(|(addr, _)| *addr))
                .await;

            if cfg.reconcile_every > 0 && n.is_multiple_of(cfg.reconcile_every) {
                // Free: `finalized` and `n` (the block just applied) are
                // both already in hand, so this adds no new trust
                // dependency (ADR-0036 §3).
                let lag = finalized.saturating_sub(n);
                let outcome = reconcile(
                    &confirm,
                    &node,
                    n,
                    lag,
                    &changed,
                    &credited,
                    cfg.complete,
                    cfg.reconcile_samples,
                    cfg.reconcile_every,
                    &mut reservoir,
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

/// The reconcile loop's view of "ask an independent source for the balance
/// of `addr` at block `n`" — implemented by [`RpcClient`] for real traffic
/// and by an in-memory stub in `tests`, which is what makes the
/// attempt-budget logic in [`sample_reference`] unit-testable with no
/// network at all. A local trait over a foreign type (`RpcClient` lives in
/// `risepir_feed`) — allowed under Rust's orphan rules, and the
/// lowest-churn way to make this one seam swappable without touching
/// `RpcClient` itself or any of its other callers.
///
/// Declared as an explicit `-> impl Future<...> + Send` rather than
/// sugared `async fn` purely so the returned future is provably `Send` —
/// required because `reconcile` is always awaited from inside
/// `follow_loop`, which `tokio::spawn` needs to be `Send`. Implementations
/// are free to use ordinary `async fn` syntax; the compiler checks the
/// desugared future against this bound.
trait ConfirmSource: Sync {
    /// `eth_getBalance(addr, block)` against the independent provider.
    fn balance_at(
        &self,
        addr: &Address,
        block: u64,
    ) -> impl std::future::Future<Output = Result<Balance, FeedError>> + Send;
}

impl ConfirmSource for RpcClient {
    async fn balance_at(&self, addr: &Address, block: u64) -> Result<Balance, FeedError> {
        RpcClient::balance_at(self, addr, block).await
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
/// preconditions and sample outcomes — no network inside this function,
/// which is what makes it unit-testable with no live dependency. This
/// classification is the core of ADR-0027 (extended by ADR-0036 with
/// `Deferred`): the old code's `if checked > 0` shortcut could not tell
/// "this block touched nothing worth sampling" apart from "every fetch
/// failed" — both are `checked == 0` — which is exactly the blind spot
/// `docs/deploy.md` records for the 2026-07-26 catch-up (685 checkpoints,
/// the independent provider refusing every archive-depth fetch, zero log
/// lines, the old `reconcile` still returning `true`).
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
    /// The block being applied is more than [`RECENT_DEPTH_BLOCKS`] behind
    /// `finalized` — a catch-up replay, exactly when the independent
    /// provider is known to refuse archive-depth reads (ADR-0036 §3). No
    /// fetch was even attempted; this is a blind checkpoint *by policy*
    /// rather than by discovering the hard way that every attempt fails.
    Deferred {
        /// How far behind `finalized` the applied block was.
        lag: u64,
    },
}

/// Classify a checkpoint from its preconditions and sample outcomes.
///
/// `lag` is checked **first**: if it exceeds [`RECENT_DEPTH_BLOCKS`], the
/// checkpoint is [`CheckpointOutcome::Deferred`] regardless of `candidates`
/// or `results` — by contract, [`reconcile`] never attempts a fetch at all
/// once it sees this, so `results` is always empty in practice on this
/// path, but the classifier itself does not depend on that being true.
///
/// Otherwise, `candidates` is the number of accounts the block actually
/// touched (before the `samples` cap), passed *separately* from
/// `results.len()` so "no candidates" (an empty block) and "candidates
/// present but none attempted" are never conflated even in principle — see
/// [`CheckpointOutcome::Empty`]'s docs. In practice `results` is only ever
/// empty here because `candidates` was zero: a positive sample cap always
/// attempts at least one candidate when one exists.
fn classify_checkpoint(candidates: usize, results: &[SampleResult], lag: u64) -> CheckpointOutcome {
    if lag > RECENT_DEPTH_BLOCKS {
        return CheckpointOutcome::Deferred { lag };
    }
    if candidates == 0 {
        return CheckpointOutcome::Empty;
    }
    let checked = results
        .iter()
        .filter(|r| **r == SampleResult::Matched)
        .count();
    if checked > 0 {
        CheckpointOutcome::Success { checked }
    } else {
        CheckpointOutcome::Dark {
            attempted: results.len(),
        }
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
/// stops on `Halted` — everything else, including a fully dark or deferred
/// checkpoint, lets following continue; see the module docs and ADR-0027 /
/// ADR-0036 for why.
enum ReconcileOutcome {
    /// Following continues. The checkpoint's classification has already
    /// been recorded into `node`'s [`risepir_http::ReconcileHealth`] and
    /// logged — including, if applicable, an escalating `CRITICAL` for a
    /// prolonged dark-or-deferred streak.
    Continued,
    /// A value mismatch or a verified-read error, from a normal candidate
    /// or from draining the deferred reservoir (ADR-0036 §4) — both halt
    /// through the exact same path. `reconcile` has already logged
    /// `CRITICAL` and marked the health record halted; the follow loop
    /// must stop.
    Halted,
}

/// Bounded FIFO of addresses seen as reconcile candidates during a blind
/// (dark or deferred) checkpoint — the backfill queue ADR-0036 §4 exists
/// for. Pure data structure, no network/`NodeState` dependency, which is
/// what makes its cap/dedup/drain behavior unit-testable directly.
///
/// A store-vs-provider comparison at the **current** checkpoint's block is
/// valid whenever it runs, regardless of which earlier block first made an
/// address a candidate — so draining this queue against today's height is
/// exactly as sound a check as a normal candidate, not a weaker one. That
/// is the whole argument for why this defers verification rather than
/// skipping it.
#[derive(Debug, Default)]
struct DeferredReservoir {
    addrs: VecDeque<Address>,
}

impl DeferredReservoir {
    /// Insert every address in `candidates` not already queued, up to
    /// [`DEFERRED_RESERVOIR_CAP`]. Once full, further inserts are silently
    /// dropped — existing entries are kept rather than evicted, so a
    /// prolonged blind spell does not thrash the queue by repeatedly
    /// discarding the oldest not-yet-verified address to make room for a
    /// newer one (that would never let the backlog actually drain).
    fn insert_many(&mut self, candidates: impl IntoIterator<Item = Address>) {
        for addr in candidates {
            if self.addrs.len() >= DEFERRED_RESERVOIR_CAP {
                break;
            }
            if !self.addrs.contains(&addr) {
                self.addrs.push_back(addr);
            }
        }
    }

    /// Pop up to `n` addresses from the front (oldest queued first) for the
    /// caller to attempt verifying now. Returns fewer than `n` if the
    /// reservoir does not hold that many.
    fn pop_front_up_to(&mut self, n: usize) -> Vec<Address> {
        let mut out = Vec::with_capacity(n.min(self.addrs.len()));
        for _ in 0..n {
            match self.addrs.pop_front() {
                Some(addr) => out.push(addr),
                None => break,
            }
        }
        out
    }

    /// Put an address whose drain attempt failed to *fetch* back at the
    /// **end** of the queue: a transient failure defers it again rather
    /// than losing it outright — "defer verification, don't skip it"
    /// applies just as much to a reservoir drain as to the original
    /// checkpoint. Bounded by the same cap as [`Self::insert_many`];
    /// dropped only if the reservoir is somehow already full, which cannot
    /// happen from this call alone (it only ever follows a pop of this
    /// same address) but is kept as a defensive bound rather than assumed.
    fn requeue(&mut self, addr: Address) {
        if self.addrs.len() < DEFERRED_RESERVOIR_CAP {
            self.addrs.push_back(addr);
        }
    }

    /// Current reservoir size — surfaced via `GET /healthz`
    /// (`reconcile_reservoir_len`, ADR-0036 §4).
    fn len(&self) -> usize {
        self.addrs.len()
    }

    fn is_empty(&self) -> bool {
        self.addrs.is_empty()
    }
}

/// Attempt reference-balance fetches for `candidates` in order, stopping
/// once either `samples` fetches have **succeeded** or `budget` fetches
/// have been **attempted** — whichever comes first (ADR-0036 §1). Returns
/// every attempted outcome, in the order attempted, index-aligned with
/// `candidates`' own prefix.
///
/// Deliberately fetch-only: it never compares a fetched balance against
/// this deployment's own store, so it has no [`risepir_http::NodeState`]
/// dependency and is unit-testable against a stub [`ConfirmSource`] with no
/// network and no PIR server at all. Splitting fetch from compare is sound
/// here specifically because a *mismatch* always halts the whole checkpoint
/// immediately (see [`reconcile`]'s docs) — so "fetched successfully" and
/// "compared and matched" are the same count for as long as the checkpoint
/// keeps running at all; the only way they would ever diverge is a
/// mismatch, which ends the checkpoint on the spot regardless of how many
/// candidates remain unattempted.
async fn sample_reference<C: ConfirmSource>(
    confirm: &C,
    block: u64,
    candidates: &[Address],
    samples: usize,
    budget: usize,
) -> Vec<Result<Balance, FeedError>> {
    let mut succeeded = 0usize;
    let mut outcomes = Vec::new();
    for addr in candidates {
        if succeeded >= samples || outcomes.len() >= budget {
            break;
        }
        let outcome = confirm.balance_at(addr, block).await;
        if outcome.is_ok() {
            succeeded += 1;
        }
        outcomes.push(outcome);
    }
    outcomes
}

/// Truncates per-sample fetch-failure logging to the first
/// [`Self::VERBATIM_CAP`] lines per checkpoint, folding every failure past
/// that into a single trailing count (ADR-0036 §5) — with (1) and (3)
/// already bounding a checkpoint to ≤16 fetch attempts, this bounds the
/// *log lines* further still, without losing the information that failures
/// happened or how many.
struct FailureLogger {
    logged: usize,
    suppressed: usize,
}

impl FailureLogger {
    /// How many individual fetch-failure lines a single checkpoint prints
    /// verbatim before folding the rest into one summary line.
    const VERBATIM_CAP: usize = 2;

    fn new() -> Self {
        Self {
            logged: 0,
            suppressed: 0,
        }
    }

    /// Record one fetch failure; prints it verbatim for the first
    /// [`Self::VERBATIM_CAP`] calls this checkpoint, silently counts it
    /// otherwise.
    fn log(&mut self, addr: &Address, block: u64, err: &FeedError) {
        if self.logged < Self::VERBATIM_CAP {
            logln!(
                "risepir-rpc mainnet: reconcile: fetch for 0x{} at {block} failed ({err}); skipping sample",
                hex20(addr)
            );
            self.logged += 1;
        } else {
            self.suppressed += 1;
        }
    }

    /// Print the trailing "...and N more" summary, if anything was
    /// suppressed. Call once, after every fetch this checkpoint has already
    /// been logged.
    fn finish(&self) {
        if self.suppressed > 0 {
            logln!(
                "risepir-rpc mainnet: reconcile: ... and {} more fetch failure(s) this checkpoint",
                self.suppressed
            );
        }
    }
}

/// One candidate's fetch-succeeded comparison against this deployment's own
/// store: [`CompareOutcome::Matched`], or [`CompareOutcome::Halted`] after
/// already logging `CRITICAL` and marking the health record halted (a
/// verified-read error or an actual value mismatch) — shared by
/// `reconcile`'s normal-candidate loop and its reservoir drain so both halt
/// through the exact same path.
enum CompareOutcome {
    Matched,
    Halted,
}

/// Runs one [`CompareOutcome`] comparison. `reservoir_entry` only changes
/// the CRITICAL message's wording (naming that the address came from the
/// deferred reservoir, ADR-0036 §4) — the halt behavior and the message's
/// shape are otherwise identical to a normal candidate's, unchanged from
/// before this ADR.
async fn compare_one(
    node: &Arc<NodeState>,
    addr: &Address,
    n: u64,
    reference: Balance,
    reservoir_entry: bool,
) -> CompareOutcome {
    let ours = match node.balance_of(&keccak256(addr)).await {
        Ok(v) => v.unwrap_or(0),
        Err(e) => {
            node.mark_reconcile_halted();
            let suffix = if reservoir_entry {
                " (this address was a deferred-reservoir entry, ADR-0036 §4)"
            } else {
                ""
            };
            critical(&format!(
                "verified read during reconcile failed: {e}{suffix}"
            ));
            return CompareOutcome::Halted;
        }
    };
    if ours != reference {
        node.mark_reconcile_halted();
        let suffix = if reservoir_entry {
            " — this address was a deferred-reservoir entry (ADR-0036 §4), verified at the current checkpoint's block"
        } else {
            ""
        };
        critical(&format!(
            "reconcile MISMATCH at block {n} for 0x{}: store says {ours} wei, independent provider says {reference} wei — \
             the feed has drifted; serving stays at the last applied block; re-bootstrap required{suffix}",
            hex20(addr)
        ));
        return CompareOutcome::Halted;
    }
    CompareOutcome::Matched
}

/// Human-readable "time since the last successful reconcile comparison",
/// shared by the dark and deferred log lines (ADR-0036) — both need
/// exactly the same "how stale is the backstop" framing.
fn since_last_success(health: &ReconcileHealth) -> String {
    if health.last_success_unix == 0 {
        "no successful comparison yet this run".to_string()
    } else {
        let elapsed = unix_now().saturating_sub(health.last_success_unix);
        format!(
            "{} since the last one, at block {}",
            format_duration_secs(elapsed),
            health.last_success_block
        )
    }
}

/// Escalates to a `CRITICAL` log once `health.consecutive_dark` crosses a
/// [`DARK_ESCALATION_THRESHOLD`] multiple ([`should_escalate`]) — shared by
/// the dark and deferred paths (ADR-0036): a deferred checkpoint counts
/// toward the same streak and must re-page the operator on exactly the
/// same cadence a genuinely-dark one would.
fn maybe_escalate(health: &ReconcileHealth, reconcile_every: u64) {
    if !should_escalate(health.consecutive_dark) {
        return;
    }
    let blocks_dark = health.consecutive_dark.saturating_mul(reconcile_every);
    // Same streak, same cadence, different *cause*. A deferred checkpoint
    // never sends a request (see the `Deferred` branch in `reconcile_at`),
    // so blaming the reference provider for a run of them is simply false —
    // and it is the likeliest escalation an operator will ever see, because
    // every deep catch-up produces one. Saying "provider appears
    // unavailable" through a routine re-bootstrap is how a CRITICAL gets
    // trained into background noise, during the exact window when the
    // ingest path really is unverified.
    let cause = if health.consecutive_deferred >= health.consecutive_dark {
        "every checkpoint in that streak was DEFERRED, not failed: this deployment is further behind \
         the finalized head than the keyless reference provider serves, so no request has been sent \
         to it at all. This is the expected shape of a deep catch-up (a re-bootstrap, or a long \
         outage) and it clears by itself once the replay reaches the provider's window — no operator \
         action, and no reason to suspect the provider"
    } else if health.consecutive_deferred > 0 {
        "that streak mixes deferred checkpoints (too far behind the reference provider to ask) with \
         attempted-and-failed ones, so both catch-up lag and a provider problem are in play"
    } else {
        "every checkpoint in that streak attempted at least one comparison and every attempt failed \
         — the independent reconcile provider appears unavailable"
    };
    critical(&format!(
        "the reconcile integrity backstop has been dark for {} checkpoint(s) (~{blocks_dark} blocks) — \
         no cross-provider comparison has succeeded since block {} — {cause}; following continues \
         regardless (a third-party outage must not become this deployment's outage), but the ingest \
         path is running unverified until this clears",
        health.consecutive_dark, health.last_success_block
    ));
}

/// Diff up to `samples` of block `n`'s own touched accounts against the
/// independent provider at height `n`, classify the checkpoint
/// ([`classify_checkpoint`]), record it into `node`'s reconcile health
/// ([`risepir_http::NodeState::record_reconcile_checkpoint`]), and log it —
/// every checkpoint, including a dark or deferred one.
///
/// `lag` is `finalized - n` at the moment this block was applied (free —
/// the follow loop already has both). When `lag` exceeds
/// [`RECENT_DEPTH_BLOCKS`] this checkpoint **defers**: no fetch is
/// attempted at all, because that lag means a catch-up replay is under
/// way, which is exactly when the independent provider is known to refuse
/// archive-depth reads (ADR-0036 §3) — attempting anyway is exactly the
/// request storm this ADR exists to stop. A deferred checkpoint is
/// recorded and escalates exactly like a dark one ([`classify_checkpoint`]);
/// its candidates are queued into `reservoir` instead of being fetched, and
/// are verified later (ADR-0036 §4) once checkpoints are no longer
/// deferred.
///
/// Otherwise, up to `samples.saturating_mul(2)` fetches are attempted
/// ([`sample_reference`], ADR-0036 §1) — bounding a fully-failing
/// checkpoint's request volume without needing the whole ~300-address
/// candidate list to be walked first. A fetch failure is not evidence of
/// anything wrong with *this* deployment; it only marks that sample
/// [`SampleResult::FetchFailed`] (logged verbatim at most
/// [`FailureLogger::VERBATIM_CAP`] times, ADR-0036 §5). A value
/// **mismatch** is CRITICAL and returns [`ReconcileOutcome::Halted`] — stop
/// following, keep serving the last good block — exactly as before this
/// change, whether the mismatch came from a normal candidate or from
/// draining `reservoir`. Everything else, including a checkpoint that is
/// *entirely* dark or deferred, returns [`ReconcileOutcome::Continued`]:
/// see the module docs for why halting on a third party's unavailability is
/// the wrong trade. A prolonged dark-or-deferred streak instead escalates
/// to a `CRITICAL` log at [`DARK_ESCALATION_THRESHOLD`] — loud, but not
/// fatal.
///
/// `reservoir` persists across calls (owned by the follow loop): every
/// blind (dark or deferred) checkpoint's candidates are queued into it
/// (bounded, ADR-0036 §4), and every non-deferred checkpoint additionally
/// drains up to [`RESERVOIR_DRAIN_PER_CHECKPOINT`] of its oldest entries,
/// verified at *this* checkpoint's block `n` — a store-vs-provider
/// comparison is valid at whatever height it actually runs, independent of
/// which earlier block first made the address a candidate.
#[allow(clippy::too_many_arguments)]
async fn reconcile<C: ConfirmSource>(
    confirm: &C,
    node: &Arc<NodeState>,
    n: u64,
    lag: u64,
    changed: &[(Address, Balance)],
    credited: &[(Address, Balance)],
    complete: bool,
    samples: usize,
    reconcile_every: u64,
    reservoir: &mut DeferredReservoir,
) -> ReconcileOutcome {
    // Tx-changed accounts are exact in both modes; credited recipients
    // are only guaranteed tracked in complete mode.
    let mut candidates: Vec<Address> = changed.iter().map(|(a, _)| *a).collect();
    if complete {
        candidates.extend(credited.iter().map(|(a, _)| *a));
    }
    candidates.dedup();
    let candidate_count = candidates.len();

    // Deferred: decided from `lag` alone, via the same pure classifier the
    // non-deferred path uses below — no fetch is attempted on this branch.
    if let CheckpointOutcome::Deferred { lag } = classify_checkpoint(candidate_count, &[], lag) {
        reservoir.insert_many(candidates);
        node.set_reservoir_len(reservoir.len() as u64);

        let health = node.record_reconcile_checkpoint(n, 0, true, true);
        let since_success = since_last_success(&health);
        logln!(
            "risepir-rpc mainnet: reconcile: block {n}: deferred — {lag} block(s) behind the finalized head, \
             deeper than the independent provider serves without a token (dark checkpoint #{} in a row); {since_success}",
            health.consecutive_dark
        );
        maybe_escalate(&health, reconcile_every);
        return ReconcileOutcome::Continued;
    }

    let budget = samples.saturating_mul(2);
    let outcomes = sample_reference(confirm, n, &candidates, samples, budget).await;

    let mut failures = FailureLogger::new();
    let mut results: Vec<SampleResult> = Vec::with_capacity(outcomes.len());
    for (addr, outcome) in candidates.iter().zip(outcomes.iter()) {
        match outcome {
            Err(e) => {
                failures.log(addr, n, e);
                results.push(SampleResult::FetchFailed);
            }
            Ok(reference) => match compare_one(node, addr, n, *reference, false).await {
                CompareOutcome::Matched => results.push(SampleResult::Matched),
                CompareOutcome::Halted => {
                    failures.finish();
                    return ReconcileOutcome::Halted;
                }
            },
        }
    }
    failures.finish();

    let outcome = classify_checkpoint(candidate_count, &results, lag);
    let (record_checked, dark) = match outcome {
        CheckpointOutcome::Empty => (0, false),
        CheckpointOutcome::Success { checked } => (checked, false),
        CheckpointOutcome::Dark { .. } => (0, true),
        CheckpointOutcome::Deferred { .. } => {
            unreachable!("lag <= RECENT_DEPTH_BLOCKS was already established above")
        }
    };
    let health = node.record_reconcile_checkpoint(n, record_checked, dark, false);

    match outcome {
        CheckpointOutcome::Empty => {
            logln!("risepir-rpc mainnet: reconcile at block {n}: no candidate accounts to check (empty block)");
        }
        CheckpointOutcome::Success { checked } => {
            logln!("risepir-rpc mainnet: reconcile at block {n}: {checked} account(s) exact vs independent provider");
        }
        CheckpointOutcome::Dark { attempted } => {
            let since_success = since_last_success(&health);
            logln!(
                "risepir-rpc mainnet: reconcile: WARNING: block {n}: {attempted} fetch(es) attempted against the \
                 independent provider, {attempted} failed (dark checkpoint #{} in a row); {since_success}",
                health.consecutive_dark
            );
            maybe_escalate(&health, reconcile_every);
        }
        CheckpointOutcome::Deferred { .. } => {
            unreachable!("lag <= RECENT_DEPTH_BLOCKS was already established above")
        }
    }

    // Drain the deferred reservoir (ADR-0036 §4): this checkpoint ran
    // normally (not deferred), so a couple of backlog addresses from an
    // earlier blind spell get a chance to verify at *this* block's height.
    let had_entries = !reservoir.is_empty();
    for addr in reservoir.pop_front_up_to(RESERVOIR_DRAIN_PER_CHECKPOINT) {
        match confirm.balance_at(&addr, n).await {
            Err(e) => {
                logln!(
                    "risepir-rpc mainnet: reconcile: deferred-reservoir fetch for 0x{} at {n} failed ({e}); requeued",
                    hex20(&addr)
                );
                reservoir.requeue(addr);
            }
            Ok(reference) => match compare_one(node, &addr, n, reference, true).await {
                CompareOutcome::Matched => node.record_reservoir_check(),
                CompareOutcome::Halted => {
                    node.set_reservoir_len(reservoir.len() as u64);
                    return ReconcileOutcome::Halted;
                }
            },
        }
    }
    node.set_reservoir_len(reservoir.len() as u64);
    if had_entries && reservoir.is_empty() {
        logln!(
            "risepir-rpc mainnet: reconcile: deferred reservoir drained to empty — every address queued during a \
             blind checkpoint has now been verified"
        );
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
    logln!("risepir-rpc mainnet: CRITICAL: {msg}");
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
        let results = [
            SampleResult::FetchFailed,
            SampleResult::FetchFailed,
            SampleResult::FetchFailed,
        ];
        let outcome = classify_checkpoint(3, &results, 0);
        assert_eq!(outcome, CheckpointOutcome::Dark { attempted: 3 });
    }

    /// A block with no candidate accounts at all (nothing touched worth
    /// sampling) must classify as `Empty`, never `Dark` — an empty block is
    /// not evidence that anything failed.
    #[test]
    fn no_candidates_classifies_as_empty() {
        let outcome = classify_checkpoint(0, &[], 0);
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
        assert_eq!(
            classify_checkpoint(0, &[SampleResult::Matched], 0),
            CheckpointOutcome::Empty
        );
    }

    /// At least one completed comparison, even amid failures, is `Success`
    /// — a mismatch (which never becomes a `SampleResult`) would already
    /// have short-circuited `reconcile` before classification runs, so any
    /// `Matched` here is trustworthy.
    #[test]
    fn one_match_among_failures_is_success_with_exact_count() {
        let results = [
            SampleResult::FetchFailed,
            SampleResult::Matched,
            SampleResult::FetchFailed,
            SampleResult::Matched,
        ];
        let outcome = classify_checkpoint(4, &results, 0);
        assert_eq!(outcome, CheckpointOutcome::Success { checked: 2 });
    }

    // ── classify_checkpoint: Deferred (ADR-0036 §3) ─────────────────────

    /// Lag beyond `RECENT_DEPTH_BLOCKS` classifies as `Deferred`,
    /// regardless of candidates/results — `reconcile` never attempts a
    /// fetch once it sees this, so `results` is always empty in practice,
    /// but the classifier itself must not depend on that being true: even
    /// a `Matched` result present must not override `Deferred`.
    #[test]
    fn lag_beyond_recent_depth_classifies_as_deferred_regardless_of_results() {
        let lag = RECENT_DEPTH_BLOCKS + 1;
        assert_eq!(
            classify_checkpoint(5, &[], lag),
            CheckpointOutcome::Deferred { lag }
        );
        assert_eq!(
            classify_checkpoint(5, &[SampleResult::Matched], lag),
            CheckpointOutcome::Deferred { lag },
            "deferred must take priority over any results that happen to be present"
        );
    }

    /// Lag exactly at the threshold is NOT deferred (only strictly beyond
    /// it) — pins the boundary rather than leaving `>` vs `>=` ambiguous.
    #[test]
    fn lag_exactly_at_threshold_is_not_deferred() {
        assert_eq!(
            classify_checkpoint(0, &[], RECENT_DEPTH_BLOCKS),
            CheckpointOutcome::Empty
        );
        assert_eq!(
            classify_checkpoint(3, &[SampleResult::Matched], RECENT_DEPTH_BLOCKS),
            CheckpointOutcome::Success { checked: 1 }
        );
    }

    /// Zero lag (fully caught up) behaves exactly as pre-ADR-0036 — the
    /// default/common case must be unaffected by the new parameter.
    #[test]
    fn zero_lag_is_never_deferred() {
        assert_eq!(classify_checkpoint(0, &[], 0), CheckpointOutcome::Empty);
    }

    // ── should_escalate ──────────────────────────────────────────────────

    /// No escalation below the threshold, ever.
    #[test]
    fn escalation_does_not_fire_before_threshold() {
        for n in 0..DARK_ESCALATION_THRESHOLD {
            assert!(
                !should_escalate(n),
                "must not escalate at consecutive_dark={n}"
            );
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
        assert!(
            (stats.mean_ms() - 20.0).abs() < 1e-9,
            "mean_ms = {}",
            stats.mean_ms()
        );
        // mean K = (100 + 200 + 300) / 3 = 200
        assert!(
            (stats.mean_k() - 200.0).abs() < 1e-9,
            "mean_k = {}",
            stats.mean_k()
        );
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

    // ── ADR-0036: attempt budget, deferral, reservoir ───────────────────
    //
    // Everything below is exercised without a network or a live `NodeState`
    // — [`ConfirmSource`] is what makes that possible: `sample_reference`
    // only ever needs *some* implementation, and a stub costs nothing.

    /// Distinct, deterministic test addresses — `Address` is `[u8; 20]`, so
    /// a plain-repeated byte only gives 256 distinct values, not enough to
    /// exceed [`DEFERRED_RESERVOIR_CAP`]; encode `i` into the low bytes
    /// instead.
    fn addr_for(i: u32) -> Address {
        let mut a = [0u8; 20];
        a[..4].copy_from_slice(&i.to_le_bytes());
        a
    }

    struct AlwaysFails;
    impl ConfirmSource for AlwaysFails {
        async fn balance_at(&self, _addr: &Address, _block: u64) -> Result<Balance, FeedError> {
            Err(FeedError::Rpc {
                method: "eth_getBalance".to_string(),
                detail: "stub: always fails".to_string(),
            })
        }
    }

    struct AlwaysSucceeds;
    impl ConfirmSource for AlwaysSucceeds {
        async fn balance_at(&self, _addr: &Address, _block: u64) -> Result<Balance, FeedError> {
            Ok(0)
        }
    }

    /// The exact behavior ADR-0036 §1 exists for: a confirm provider that
    /// always fails must stop at the attempt **budget**, never walk the
    /// whole candidate list. Pre-ADR-0036, this would have produced 300
    /// attempts (one `logln!` and one HTTP request each); afterward,
    /// exactly `budget` (16 at the real default `samples = 8`).
    #[tokio::test]
    async fn attempt_budget_bounds_requests_when_every_fetch_fails() {
        let candidates: Vec<Address> = (0..300u32).map(addr_for).collect();
        let samples = 8usize;
        let budget = samples.saturating_mul(2);
        let outcomes = sample_reference(&AlwaysFails, 1, &candidates, samples, budget).await;
        assert_eq!(
            outcomes.len(),
            budget,
            "must stop at the attempt budget ({budget}), not exhaust all {} candidates",
            candidates.len()
        );
        assert!(outcomes.iter().all(Result::is_err));
    }

    /// The other stopping condition: when fetches succeed, sampling stops
    /// once `samples` of them have, well before the (larger) budget.
    #[tokio::test]
    async fn attempt_budget_stops_at_samples_when_fetches_succeed() {
        let candidates: Vec<Address> = (0..300u32).map(addr_for).collect();
        let samples = 8usize;
        let budget = samples.saturating_mul(2);
        let outcomes = sample_reference(&AlwaysSucceeds, 1, &candidates, samples, budget).await;
        assert_eq!(
            outcomes.len(),
            samples,
            "must stop once `samples` fetches have succeeded"
        );
        assert!(outcomes.iter().all(Result::is_ok));
    }

    /// Fewer candidates than either bound: sampling stops when candidates
    /// run out, attempting neither `samples` successes nor the full budget.
    #[tokio::test]
    async fn attempt_budget_stops_early_when_candidates_run_out() {
        let candidates: Vec<Address> = (0..3u32).map(addr_for).collect();
        let outcomes = sample_reference(&AlwaysFails, 1, &candidates, 8, 16).await;
        assert_eq!(outcomes.len(), 3);
    }

    // ── DeferredReservoir (ADR-0036 §4) ─────────────────────────────────

    #[test]
    fn reservoir_caps_at_capacity_and_keeps_the_earliest_entries() {
        let mut r = DeferredReservoir::default();
        let addrs: Vec<Address> = (0..(DEFERRED_RESERVOIR_CAP as u32 + 50))
            .map(addr_for)
            .collect();
        r.insert_many(addrs.iter().copied());
        assert_eq!(
            r.len(),
            DEFERRED_RESERVOIR_CAP,
            "must not grow past the cap"
        );
        // The FIRST entries are kept, not the last -- no eviction/thrash of
        // already-queued (older) addresses to make room for newer ones.
        assert_eq!(r.pop_front_up_to(1), vec![addr_for(0)]);
    }

    #[test]
    fn reservoir_dedups_on_insert() {
        let mut r = DeferredReservoir::default();
        r.insert_many([addr_for(1), addr_for(1), addr_for(2)]);
        assert_eq!(r.len(), 2);
    }

    #[test]
    fn reservoir_pop_front_is_fifo_and_bounded_by_whats_available() {
        let mut r = DeferredReservoir::default();
        r.insert_many([addr_for(1), addr_for(2), addr_for(3)]);
        assert_eq!(r.pop_front_up_to(2), vec![addr_for(1), addr_for(2)]);
        assert_eq!(r.len(), 1);
        // Asking for more than what remains returns only what's there,
        // rather than panicking or padding.
        assert_eq!(r.pop_front_up_to(5), vec![addr_for(3)]);
        assert!(r.is_empty());
    }

    /// A drain attempt whose fetch fails is requeued at the **end** —
    /// deferred again, not lost ("defer verification, don't skip it"
    /// applies to a reservoir drain too).
    #[test]
    fn reservoir_requeue_puts_the_address_back_at_the_end() {
        let mut r = DeferredReservoir::default();
        r.insert_many([addr_for(1), addr_for(2)]);
        let popped = r.pop_front_up_to(1);
        assert_eq!(popped, vec![addr_for(1)]);
        r.requeue(popped[0]);
        assert_eq!(r.pop_front_up_to(2), vec![addr_for(2), addr_for(1)]);
    }

    // ── FailureLogger (ADR-0036 §5) ──────────────────────────────────────

    /// Counting behavior only (the printed lines themselves are exactly
    /// what the module docs describe and are not worth capturing stderr
    /// to re-verify): the first `VERBATIM_CAP` calls are not suppressed,
    /// everything past that is.
    #[test]
    fn failure_logger_suppresses_past_the_verbatim_cap() {
        let addr = addr_for(0);
        let err = FeedError::Rpc {
            method: "eth_getBalance".to_string(),
            detail: "stub".to_string(),
        };
        let mut logger = FailureLogger::new();
        assert_eq!(
            FailureLogger::VERBATIM_CAP,
            2,
            "test assumes the documented default of 2"
        );
        for _ in 0..5 {
            logger.log(&addr, 1, &err);
        }
        assert_eq!(logger.logged, 2);
        assert_eq!(logger.suppressed, 3);
        logger.finish(); // must not panic; nothing to assert on stderr itself
    }

    #[test]
    fn failure_logger_reports_nothing_suppressed_under_the_cap() {
        let addr = addr_for(0);
        let err = FeedError::Rpc {
            method: "eth_getBalance".to_string(),
            detail: "stub".to_string(),
        };
        let mut logger = FailureLogger::new();
        logger.log(&addr, 1, &err);
        assert_eq!(logger.logged, 1);
        assert_eq!(logger.suppressed, 0);
    }
}
