//! axum HTTP server exposing the RisePIR endpoints
//! (`docs/plan.md` §3.4, ADR-0006) over a [`tokio::sync::RwLock`]-guarded
//! [`RisePirServer`] (ADR-0010: the concrete server type is
//! auto-`Send + Sync`, so the lock gives concurrent readers on the hot
//! `/answer` path).

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, RawQuery, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
// This file holds both kinds of mutex, on purpose, and the distinction is
// load-bearing enough to be visible at every use site rather than inferred
// from an import list. Bare `Mutex` is `std`'s, for critical sections that
// are a few field accesses with no `.await` in them (`reconcile`);
// `AsyncMutex` is tokio's, for a guard that *is* held across an await
// point (`setup_cache`, which awaits `inner.read()` inside it). Getting
// these the wrong way round is either a needlessly async lock or a
// blocking one held across a yield — the second stalls the executor.
use tokio::sync::{Mutex as AsyncMutex, RwLock};
use tower_http::timeout::TimeoutLayer;

use ikpir_common::backend::simple::SimpleServerParams;
use ikpir_common::SimplePirBackend;
use risepir_proto::{codec, BlockDelta, BlockUpdate};
use risepir_server::{DeltaRing, RisePirServer, ServerError};
use segmented_cuckoo::Segmented2aryScheme;

use crate::wire;

/// Maximum accepted `POST /answer` body size, in bytes. Generous but
/// finite — bounds request-buffering memory before any wire decoding even
/// starts (`docs/plan.md`'s "malformed/hostile bytes" hazard applies to
/// the transport layer too, not just the codec: an attacker should not be
/// able to force an unbounded read just by sending a very long body).
pub const MAX_ANSWER_BODY_BYTES: usize = 8 * 1024 * 1024; // 8 MiB

/// Per-request service timeout. Bounds both a slow-trickled request body
/// and a handler stuck behind the write lock; expiry is a clean `408`,
/// never a hung connection held open indefinitely (threat model §3). The
/// response *body* transfer is not under this clock — every response here
/// is fully materialized (or, for `/setup`, refcount-cloned from
/// [`NodeState`]'s cache — ADR-0028) before it is sent, so a slow download
/// only occupies a socket, not a handler. That is still true of a `/setup`
/// cache *regeneration*, the one handler on this router that now does
/// nontrivial work: encoding a fresh bundle is bounded, in-memory work that
/// finishes long before this clock could matter. It is the multi-minute
/// wire transfer to a slow client — measured 7m46s for the real
/// complete-mainnet bundle — that never touches this timeout at all, on
/// purpose, because it happens entirely after the handler has returned.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// How many recently-touched addresses `GET /recent` retains
/// ([`NodeState::note_recent`]). Small on purpose: this is a "here is
/// something you can actually ask about" affordance for the browser front
/// end in a partial deployment, not a chain index.
pub const RECENT_CAPACITY: usize = 128;

/// Response header carrying [`NodeState::epoch`] (the hint-lineage token,
/// [`wire::lineage_epoch`], ADR-0033) on `GET /setup` and `GET /head`.
/// Clients echo the value back as the `?epoch=` query param on `/sync`,
/// `/answer`, and `/delta/{block}`.
pub const HEADER_EPOCH: &str = "x-risepir-epoch";

/// Response header carrying the deployment mode (`"0"` partial / `"1"`
/// complete) on `GET /setup` — the same flag `GET /mode` serves as a body
/// byte, duplicated onto the setup response so a client can read the
/// mode and the bundle from one atomic response instead of racing two
/// requests against a server restart (ADR-0033; the race is the mixed
/// `strict_not_found`/hint pair ADR-0029's notes accept).
pub const HEADER_MODE: &str = "x-risepir-mode";

/// Point-in-time snapshot of the cross-provider reconciliation check's own
/// health — what `GET /healthz` reports (see that handler's doc comment for
/// the wire format) and what the follow loop in `risepir-rpc` updates after
/// every checkpoint via [`NodeState::record_reconcile_checkpoint`].
///
/// # Why this exists
///
/// The integrity backstop (`docs/threat-model.md` §6) is *sampled*
/// cross-provider comparison, not prevention, and it can itself go dark: on
/// 2026-07-26 it ran with zero completed comparisons for ~2 hours during a
/// catch-up (the independent provider refuses archive-depth fetches), and
/// nothing surfaced it — not `/healthz`, not `/mode`, not the startup
/// banner. The old code's summary line was gated on `checked > 0`, so a
/// checkpoint that compared several accounts and found them exact was
/// indistinguishable, to any monitor, from a checkpoint where every fetch
/// failed: both logged nothing and both left the caller reporting success.
/// This type exists so that distinction is observable instead of inferred
/// (ADR-0027).
#[derive(Clone, Copy, Debug, Default)]
pub struct ReconcileHealth {
    /// Whether reconciliation runs at all (`reconcile_every > 0`). `false`
    /// for any deployment that never calls
    /// [`NodeState::set_reconcile_configured`] — mock/demo included — and
    /// deliberately not inferred from any other field: an unconfigured
    /// deployment must read as "not configured", never silently as
    /// "healthy" just because every counter happens to be zero.
    pub configured: bool,
    /// Block number of the most recent checkpoint attempted (dark, empty,
    /// or successful) — `0` if none has run yet this process.
    pub last_checkpoint_block: u64,
    /// Unix timestamp (seconds) of the most recent checkpoint attempted —
    /// `0` if none has run yet this process.
    pub last_checkpoint_unix: u64,
    /// Block number of the most recent checkpoint with at least one
    /// completed comparison — `0` if none has ever succeeded this process.
    pub last_success_block: u64,
    /// Unix timestamp (seconds) of the most recent checkpoint with at least
    /// one completed comparison — `0` if none has ever succeeded this
    /// process.
    pub last_success_unix: u64,
    /// Total individual account comparisons completed, summed across every
    /// checkpoint this process has run.
    pub comparisons_total: u64,
    /// Total checkpoints attempted (dark, empty, or successful) this
    /// process.
    pub checkpoints_total: u64,
    /// Consecutive checkpoints that attempted at least one comparison and
    /// had *every* attempt fail. A checkpoint with no candidate accounts at
    /// all (an empty block — nothing was there to check) leaves this
    /// unchanged rather than incrementing it; any checkpoint with at least
    /// one completed comparison resets it to `0`. See
    /// [`NodeState::record_reconcile_checkpoint`].
    pub consecutive_dark: u64,
    /// Whether a value mismatch has permanently halted the follow loop.
    /// Serving continues at the last good block regardless (never-wrong-
    /// answer, not never-stale) — this field only reports that the
    /// integrity backstop is the reason nothing new is being applied.
    pub halted: bool,
    /// Total checkpoints that were **deferred** (ADR-0036 §3) — skipped
    /// fetching entirely because the block being applied was more than
    /// `RECENT_DEPTH_BLOCKS` behind `finalized`. These also count toward
    /// `consecutive_dark`; this field exists purely so a deferred
    /// checkpoint is distinguishable, in hindsight, from one that actually
    /// attempted and failed.
    pub deferred_total: u64,
    /// Total deferred-reservoir addresses successfully verified (ADR-0036
    /// §4) — the same "completed comparison" convention `comparisons_total`
    /// uses, counted separately since these are backfilled candidates from
    /// an earlier blind checkpoint rather than the current one's own.
    pub reservoir_checks_total: u64,
    /// Current size of the deferred-reservoir backfill queue (ADR-0036
    /// §4) — a gauge, not a counter: it grows as blind checkpoints queue
    /// candidates and shrinks as later checkpoints drain them.
    pub reservoir_len: u64,
}

fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Everything the lock guards: the PIR server, its sliding delta-ring
/// window, and a bounded per-block delta index for immutable
/// `GET /delta/{block}` lookups. `per_block` exists because [`DeltaRing`]
/// only answers coalesced *ranges* (`range(from, to)`), not "give me
/// exactly block N's own delta" — ADR-0006 wants deltas served as
/// immutable per-block objects, so this index keeps the individual
/// per-block bytes retrievable, capped to exactly the ring's own retention
/// window (see [`NodeState::apply_block`]).
struct Inner {
    server: RisePirServer<Segmented2aryScheme, SimplePirBackend>,
    ring: DeltaRing,
    per_block: BTreeMap<u64, BlockDelta>,
}

/// One fully wire-encoded `GET /setup` response, cached by the block its
/// [`risepir_server::SetupBundle`] was pinned at (ADR-0028). `bytes` is
/// exactly what [`wire::encode_setup`] produced — cloning it (`bytes::Bytes`,
/// re-exported as [`axum::body::Bytes`]) is a refcount bump, never a copy,
/// which is what makes handing every concurrent `/setup` caller its own
/// clone O(1) in the number of callers instead of O(N) in encoded bytes.
struct CachedSetup {
    /// The block this bundle was pinned at when it was encoded — compared
    /// against the server's *current* head to decide whether the bytes are
    /// still safe to serve (see [`NodeState::setup_bytes`]).
    block: u64,
    /// The full `GET /setup` response body.
    bytes: Bytes,
}

/// Shared server state behind `Arc` (ADR-0010).
pub struct NodeState {
    inner: RwLock<Inner>,
    /// Whether the served set is the *complete* nonzero-balance universe.
    /// Served to clients via `GET /mode` because the `NotFound` policy
    /// hangs on it (complete ⇒ absence is exactly `0x0`, ADR-0015;
    /// partial ⇒ absence is *unknown* and must error) — a remote front
    /// end guessing this flag would be a silent-wrong-answer bug.
    complete: bool,
    /// Per-segment SimplePIR reshape geometry (`reshape_rows` /
    /// `reshape_row_width`), cached once at construction.
    ///
    /// `RisePirServer` exposes no lighter-weight accessor than
    /// [`RisePirServer::setup`], which also clones every segment's *hint*
    /// (potentially tens of MB) — far too expensive to pay on every
    /// `/answer` request just to learn a handful of `u32` reshape
    /// dimensions. Those dimensions never change across
    /// [`RisePirServer::apply_block`] calls (only a `full_rebuild`, which
    /// this server never performs, would change them), so paying the
    /// `.setup()` cost once here and keeping only the cheap
    /// `backend_params` — never the hints — is sound.
    ///
    /// This is what lets [`wire::decode_query_bundle`] /
    /// [`wire::decode_response_bundle`] bound each segment's `Vec<u32>` to
    /// its *exact* expected length instead of merely "however many 4-byte
    /// groups fit the body" — see those functions' docs for the
    /// out-of-bounds server/client panic that exactness closes.
    backend_params: Vec<SimpleServerParams>,
    /// Recently-touched **addresses** (not key hashes), newest last, for
    /// `GET /recent`.
    ///
    /// # Why this exists, and why it costs no privacy
    ///
    /// A partial deployment (ADR-0015/0017) only knows accounts touched
    /// since it bootstrapped, so a visitor typing an arbitrary address
    /// gets an honest "not in the tracked set" error and learns nothing
    /// about the system. This gives the front end a handful of addresses
    /// that *are* in the set, so the demo can demonstrate something.
    ///
    /// It leaks nothing: these are addresses from recent blocks, which is
    /// public chain data anyone can read from any node, and it is served
    /// to everyone identically. Crucially it does not weaken the PIR
    /// property — fetching the list is a separate, address-free request,
    /// and the server still cannot tell which entry (if any) the visitor
    /// then queried. That distinction is stated on the page rather than
    /// left for the reader to work out.
    ///
    /// Separate lock from `inner`: it is written by the block-follow
    /// driver and read by `/recent`, neither of which should ever wait on
    /// the PIR server's own lock.
    recent: RwLock<VecDeque<[u8; 20]>>,

    /// Cross-provider reconciliation health — see [`ReconcileHealth`].
    ///
    /// Same reasoning as `recent`'s own lock, one step further: a
    /// `std::sync::Mutex` rather than the async `tokio::sync::RwLock`
    /// `recent` uses, because every critical section here is a handful of
    /// field writes/reads with no `.await` inside it (tokio's own guidance
    /// is that a `std` mutex held only that briefly is the right tool, not
    /// the async one). Written by the follow loop in `risepir-rpc` after
    /// every checkpoint; read by `GET /healthz`. Never inside `inner`'s
    /// lock — a `/healthz` probe must not queue behind `/answer` traffic
    /// just to report a handful of counters.
    reconcile: Mutex<ReconcileHealth>,

    /// The shared `GET /setup` response cache (ADR-0028) — `None` until the
    /// first `/setup` request populates it. A **separate** lock from
    /// `inner`, same reasoning as `recent` above, with one addition: lock
    /// *order* matters here, because regenerating the cache needs both
    /// locks at once. [`Self::setup_bytes`] always takes `setup_cache`
    /// first and `inner` second, and nothing in this crate ever acquires
    /// them the other way round — reversing that order on some future
    /// caller would be a deadlock waiting to happen.
    ///
    /// A mutex, not another `RwLock`: every caller that finds the cache
    /// stale must serialize behind whichever caller is already
    /// regenerating it (two concurrent regenerations would just waste a
    /// second ~831 MB encode), so there is no useful read/write distinction
    /// to preserve the way there is for `inner`. And tokio's rather than
    /// `std`'s — unlike `reconcile` above, this guard is held across an
    /// `.await` (the `inner.read()` inside [`Self::setup_bytes`]), which a
    /// blocking mutex must never be.
    setup_cache: AsyncMutex<Option<CachedSetup>>,

    /// How many times [`Self::setup_bytes`] has actually (re)encoded a
    /// bundle, as opposed to serving an existing cache hit. Test-only
    /// instrumentation (see [`Self::setup_generation`]); nothing in this
    /// crate's own request handling reads it.
    setup_generation: AtomicUsize,

    /// This deployment's hint-lineage identity
    /// ([`wire::lineage_epoch`], ADR-0033), fixed at construction: a pure
    /// function of the per-segment LWE seeds (random per bootstrap,
    /// persisted in the state file), so it is stable across restarts of
    /// the *same* lineage and differs across re-bootstraps.
    ///
    /// `GET /sync`, `POST /answer`, and `GET /delta/{block}` all require
    /// the caller to present this value and refuse to serve across a
    /// mismatch. Without that check, a client holding a hint from a
    /// *previous* bootstrap could catch a `200` from `/sync` whenever
    /// this process's replay head passes within ring range of the
    /// client's pinned block — and mismatched-lineage deltas applied to
    /// that hint make the fingerprint scan miss, which a complete-mode
    /// client maps to `0x0`: a silently wrong balance, the one absolutely
    /// forbidden failure. Layouts genuinely differ per bootstrap
    /// (`segmented_cuckoo` picks slots and eviction victims via
    /// `rand::rng()`), so this is a real, reachable path, not
    /// belt-and-braces.
    epoch: String,
}

impl NodeState {
    /// Wrap a freshly-built [`RisePirServer`] plus an (initially empty)
    /// [`DeltaRing`] for HTTP serving. The per-block delta index starts
    /// empty and grows as [`Self::apply_block`] is called. `complete`
    /// declares whether the served set is the complete nonzero-balance
    /// universe — see the field's docs; a mock deployment's synthetic
    /// universe is complete by construction.
    pub fn new(
        server: RisePirServer<Segmented2aryScheme, SimplePirBackend>,
        ring: DeltaRing,
        complete: bool,
    ) -> Self {
        // One-time cost, paid once at startup — see the field's docs.
        let backend_params = server.setup().backend_params;
        let epoch = wire::lineage_epoch(&backend_params);
        Self {
            inner: RwLock::new(Inner {
                server,
                ring,
                per_block: BTreeMap::new(),
            }),
            backend_params,
            complete,
            recent: RwLock::new(VecDeque::new()),
            reconcile: Mutex::new(ReconcileHealth::default()),
            setup_cache: AsyncMutex::new(None),
            setup_generation: AtomicUsize::new(0),
            epoch,
        }
    }

    /// This deployment's hint-lineage epoch — see the field's docs and
    /// ADR-0033. Fixed for the process lifetime.
    pub fn epoch(&self) -> &str {
        &self.epoch
    }

    /// Record addresses this deployment now tracks, for `GET /recent` —
    /// see the field's docs. Newest win; the buffer is capped at
    /// [`RECENT_CAPACITY`] and duplicates are collapsed so a repeatedly
    /// active account cannot crowd the list out.
    ///
    /// Called by the block-follow driver with the block's own touched
    /// addresses (`risepir-rpc`'s mainnet loop), or once at startup with a
    /// fixed set (the mock demo). Never exposed over HTTP: nothing a
    /// client sends can put an address in this list.
    pub async fn note_recent(&self, addrs: impl IntoIterator<Item = [u8; 20]>) {
        let mut recent = self.recent.write().await;
        for addr in addrs {
            if let Some(pos) = recent.iter().position(|a| *a == addr) {
                recent.remove(pos);
            }
            recent.push_back(addr);
            while recent.len() > RECENT_CAPACITY {
                recent.pop_front();
            }
        }
    }

    /// Declare whether this deployment reconciles at all
    /// (`reconcile_every > 0`). Called once at startup — before the PIR
    /// listener starts accepting requests, so `GET /healthz` never has a
    /// window where it could observe a stale "not configured" for a
    /// deployment that actually does reconcile.
    ///
    /// A mock/demo deployment (`risepir-rpc demo`) never calls this, which
    /// is the honest state: it never reconciles, and `/healthz` must say so
    /// explicitly (`reconcile_configured=0`) rather than have the field
    /// silently absent — an absent field reads as "healthy" to a monitor
    /// that does not know better, exactly the failure this type exists to
    /// close (ADR-0027).
    pub fn set_reconcile_configured(&self, configured: bool) {
        self.reconcile_lock().configured = configured;
    }

    /// Record one reconciliation checkpoint's outcome — called by the
    /// follow loop in `risepir-rpc` after every `reconcile_every`-th block
    /// — and return the freshly updated snapshot, so the caller can decide
    /// whether to escalate without a second lock acquisition.
    ///
    /// `checked` is the number of comparisons that actually completed this
    /// checkpoint; `dark` is whether at least one comparison was attempted
    /// and *every* attempt failed; `deferred` is whether this checkpoint
    /// skipped fetching entirely because the block was too far behind
    /// `finalized` (ADR-0036 §3). **A checkpoint with no candidate accounts
    /// at all must pass `checked = 0, dark = false, deferred = false`** —
    /// that is what leaves [`ReconcileHealth::consecutive_dark`] unchanged
    /// rather than incrementing it. Conflating "nothing to check" with
    /// "checked and every attempt failed" was the root cause of the
    /// 2026-07-26 blind spot this type exists to close — see the struct's
    /// docs. `deferred` bumps [`ReconcileHealth::deferred_total`] and, like
    /// `dark`, counts toward `consecutive_dark` — a deferred checkpoint is
    /// blind by policy rather than by a failed attempt, but it is exactly
    /// as blind, and ADR-0027's escalation must treat it that way.
    ///
    /// Any checkpoint with `checked > 0` resets `consecutive_dark` to `0`
    /// and advances `last_success_block`/`last_success_unix`, regardless of
    /// `dark`/`deferred` (they are mutually exclusive with `checked > 0` in
    /// practice, but `checked > 0` wins if a caller ever passed both).
    pub fn record_reconcile_checkpoint(&self, block: u64, checked: usize, dark: bool, deferred: bool) -> ReconcileHealth {
        let now = unix_now();
        let mut h = self.reconcile_lock();
        h.last_checkpoint_block = block;
        h.last_checkpoint_unix = now;
        h.checkpoints_total += 1;
        h.comparisons_total += checked as u64;
        if deferred {
            h.deferred_total += 1;
        }
        if checked > 0 {
            h.last_success_block = block;
            h.last_success_unix = now;
            h.consecutive_dark = 0;
        } else if dark || deferred {
            h.consecutive_dark += 1;
        }
        // else: an empty-block checkpoint — `consecutive_dark` is left
        // exactly as it was; see the field's docs.
        *h
    }

    /// Record one deferred-reservoir address successfully verified
    /// (ADR-0036 §4) — the same "completed comparison" convention
    /// [`ReconcileHealth::comparisons_total`] uses, counted separately into
    /// [`ReconcileHealth::reservoir_checks_total`] since these are
    /// backfilled candidates from an earlier blind checkpoint rather than
    /// the current one's own.
    pub fn record_reservoir_check(&self) {
        self.reconcile_lock().reservoir_checks_total += 1;
    }

    /// Update the deferred-reservoir size gauge
    /// ([`ReconcileHealth::reservoir_len`]) — called by the follow loop in
    /// `risepir-rpc` whenever it changes (a blind checkpoint enqueues
    /// candidates, a non-deferred one drains them).
    pub fn set_reservoir_len(&self, len: u64) {
        self.reconcile_lock().reservoir_len = len;
    }

    /// Record that a value mismatch (or a verified-read error) has
    /// permanently halted the follow loop. Surfaced via `/healthz`
    /// (`reconcile_halted=1`) even though the PIR server keeps serving its
    /// last good block — never-wrong-answer, not never-stale.
    pub fn mark_reconcile_halted(&self) {
        self.reconcile_lock().halted = true;
    }

    /// Snapshot of the current reconciliation health — what `GET /healthz`
    /// reports.
    pub fn reconcile_health(&self) -> ReconcileHealth {
        *self.reconcile_lock()
    }

    /// Locks `reconcile`, recovering the inner value on poison rather than
    /// panicking. Every critical section behind this lock is a handful of
    /// infallible field updates, so poisoning is not expected in practice —
    /// but `GET /healthz` is a liveness probe, and a probe handler must
    /// never itself panic (this crate's panic-free discipline applies here
    /// too, not just to adversarial wire input): best-effort stale-but-safe
    /// counters beat failing the request.
    fn reconcile_lock(&self) -> std::sync::MutexGuard<'_, ReconcileHealth> {
        self.reconcile.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Apply one block: the sole writer path. Applies to the server,
    /// pushes the resulting delta to the ring, and indexes it by block
    /// number — all under a single write-lock acquisition, so (per
    /// ADR-0010) `apply_block` and any concurrent `/answer` are atomic
    /// with respect to each other. Not exposed over HTTP: the
    /// block-following driver (outside this crate's Stage-0.3 scope) calls
    /// this directly.
    ///
    /// After indexing, evicts every `per_block` entry older than the
    /// ring's own new [`DeltaRing::oldest`] — keeping `per_block`'s
    /// retention exactly in sync with the ring's, with no separate
    /// capacity constant to drift out of agreement.
    ///
    /// # Returns
    ///
    /// `(delta, patch_time)` — both of this method's two callers-worth of
    /// information, because the follow loop needs each for a different
    /// reason and neither can cheaply re-derive the other.
    ///
    /// `delta` is the [`BlockDelta`] this block produced (one extra clone
    /// of the small delta beyond what the ring/`per_block` bookkeeping
    /// already needed) — the caller (`risepir-rpc`'s mainnet follow loop)
    /// hands it straight to the state-journal appender
    /// (`docs/adr/README.md` ADR-0026) without having to re-derive it.
    ///
    /// `patch_time` is the wall-clock time of the inner
    /// `RisePirServer::apply_block` call *and nothing else* — timing
    /// starts after the write lock on `self.inner` is already held, so
    /// lock-*wait* time (queueing behind whatever held the lock before
    /// this call got it) is deliberately excluded. This distinction is the
    /// whole point of the measurement: at the complete mainnet set a slow
    /// concurrent `/answer` can hold the *read* lock long enough to
    /// dominate a naive "time this whole method from entry" measurement,
    /// which would then be reporting queueing delay under someone else's
    /// name rather than the hint-patch cost itself. The ring-push and
    /// `per_block`-index bookkeeping after the timed call are cheap by
    /// comparison and deliberately excluded too. This is the per-block
    /// patch time `docs/numbers.md` §2/§6/§7 discusses, measured here on
    /// the real deployment rather than a bench harness.
    ///
    /// # Errors
    ///
    /// Whatever `RisePirServer::apply_block` returns — see
    /// [`ServerError`]. On `Err`, nothing is pushed to the ring or
    /// `per_block` (matching that method's own "no partial delta leaks"
    /// guarantee), and neither a delta nor a duration is returned — there
    /// was no patch to journal and nothing meaningful to time.
    pub async fn apply_block(
        &self,
        update: &BlockUpdate,
    ) -> Result<(BlockDelta, Duration), ServerError> {
        let mut inner = self.inner.write().await;
        let t0 = Instant::now();
        let delta = inner.server.apply_block(update)?;
        let patch_time = t0.elapsed();
        inner.ring.push(delta.clone());
        inner.per_block.insert(delta.block, delta.clone());
        if let Some(oldest) = inner.ring.oldest() {
            let stale: Vec<u64> = inner.per_block.range(..oldest).map(|(b, _)| *b).collect();
            for b in stale {
                inner.per_block.remove(&b);
            }
        }
        Ok((delta, patch_time))
    }

    /// Seeds the ring / per-block delta index from a journal-restore
    /// replay (`docs/adr/README.md` ADR-0026), so `GET /delta/{block}`
    /// and `GET /sync` cover the replayed tail immediately instead of
    /// waiting for the follow loop to re-derive that history one live
    /// block at a time.
    ///
    /// # Contract — restore-mode only
    ///
    /// `deltas` must already be in ascending block order and must all be
    /// `<=` the server's current head — which holds by construction for a
    /// journal-restore caller, since these are exactly the deltas that
    /// were just replayed *into* that same head
    /// (`crate::state::load_with_journal_restore`'s `tail_deltas`).
    /// Calling this with deltas ahead of the head, out of order, or with a
    /// gap would desynchronize the ring from the server it is meant to
    /// describe; nothing here re-validates that, since the one caller this
    /// method exists for cannot violate it.
    pub async fn seed_history(&self, deltas: Vec<BlockDelta>) {
        let mut inner = self.inner.write().await;
        for delta in deltas {
            inner.ring.push(delta.clone());
            inner.per_block.insert(delta.block, delta);
        }
        if let Some(oldest) = inner.ring.oldest() {
            let stale: Vec<u64> = inner.per_block.range(..oldest).map(|(b, _)| *b).collect();
            for b in stale {
                inner.per_block.remove(&b);
            }
        }
    }

    /// Verified read of the balance currently stored for `key`
    /// ([`RisePirServer::balance_of`]), under the read lock — what the
    /// reconciliation loop diffs against an independent reference RPC
    /// (`docs/sync.md`).
    pub async fn balance_of(
        &self,
        key: &risepir_proto::AddressHash,
    ) -> Result<Option<risepir_proto::Balance>, ServerError> {
        self.inner.read().await.server.balance_of(key)
    }

    /// Run `f` over the wrapped server under the read lock — the escape
    /// hatch state persistence uses to snapshot cells/setup/num_items
    /// atomically with respect to the block-follow writer, without this
    /// crate having to know any state-file format.
    pub async fn with_server<R>(
        &self,
        f: impl FnOnce(&RisePirServer<Segmented2aryScheme, SimplePirBackend>) -> R,
    ) -> R {
        f(&self.inner.read().await.server)
    }

    /// The current `GET /setup` response: a cache hit if what is stored is
    /// still safe to serve, a fresh encode otherwise (ADR-0028). Returns
    /// the shared [`Bytes`] (cloning it is a refcount bump, never a copy)
    /// together with the block it is pinned at, so the caller can build an
    /// `ETag` without re-decoding the body.
    ///
    /// # The freshness rule
    ///
    /// A bundle pinned at block `B` is only safe to serve while a client
    /// that bootstraps from it can still catch up via
    /// `GET /sync?from=B&to=<head>` — which needs every block in
    /// `B+1..=head` retained in the ring (see [`DeltaRing::range`]). This
    /// reuses the cached bytes only while
    /// `server.block() - cached.block <= ring.capacity() / 8`.
    ///
    /// An **eighth** of the ring, revised down from the half this shipped
    /// with: the budget a client needs is not measured in blocks but in
    /// the *time* to download ~831 MB (~8 minutes measured end to end),
    /// and how many blocks that costs depends on how fast the server is
    /// advancing. Half the ring priced it at steady state (~5 blocks/min:
    /// 300 blocks ≈ 1 h of margin — plenty) — but the moment that
    /// actually matters is a catch-up replay (~50 blocks/min, ADR-0029's
    /// own motivating case), where a half-window-stale bundle leaves only
    /// ~6 minutes of window against that 8-minute download: the freshly
    /// bootstrapped client stalls *again*, at 831 MB per attempt. At an
    /// eighth (75 blocks on the deployed 600-block ring), the served
    /// bundle is at most ~90 s stale even at replay speed, leaving ~10.5
    /// minutes of window there (~1 h 45 min at steady state), and the
    /// encode still amortizes over ~15 min of steady-state chain time
    /// rather than regenerating on every block (~10 s CPU per encode at
    /// the complete set — ~1% of steady-state wall time, ~11% during a
    /// replay, both acceptable for making the replay case actually
    /// converge).
    ///
    /// # Locking
    ///
    /// Takes `setup_cache` **then** `inner`, never the other order — see
    /// that field's docs. Holds `setup_cache` for the whole call,
    /// including a stale/absent cache's full regeneration: this is
    /// double-checked locking applied to a single mutex rather than a
    /// fast/slow pair of locks. A second caller that arrives while a
    /// regeneration is already in flight simply waits its turn on
    /// `setup_cache`; once it gets in, its own freshness check finds the
    /// bundle the first caller just produced and returns immediately
    /// instead of encoding a second time. Concurrent `/setup` callers
    /// blocking behind one encode is the intended trade — they then all
    /// get the fresh bytes rather than each paying their own ~831 MB clone
    /// plus encode.
    async fn setup_bytes(&self) -> (Bytes, u64) {
        let mut cache = self.setup_cache.lock().await;

        let inner = self.inner.read().await;
        let head = inner.server.block();
        // An eighth of the ring, not half — see the freshness-rule docs
        // above for the replay-speed arithmetic that revised this down.
        let fresh_window = inner.ring.capacity() as u64 / 8;

        if let Some(cached) = cache.as_ref() {
            if head.saturating_sub(cached.block) <= fresh_window {
                let bytes = cached.bytes.clone();
                let block = cached.block;
                drop(inner);
                return (bytes, block);
            }
        }

        // Absent or stale: regenerate now, still holding both locks — see
        // this method's docs for why that is the intended trade rather
        // than a bug.
        let bundle = inner.server.setup();
        drop(inner);
        let bytes = Bytes::from(wire::encode_setup(&bundle));
        let block = bundle.block;
        self.setup_generation.fetch_add(1, Ordering::SeqCst);
        *cache = Some(CachedSetup { block, bytes: bytes.clone() });
        (bytes, block)
    }

    /// Test-only: how many times [`Self::setup_bytes`] has actually
    /// (re)encoded a bundle rather than served an existing cache hit — lets
    /// a test assert "the encode ran exactly once" directly instead of
    /// inferring it from timing. `#[doc(hidden)]` and not part of this
    /// crate's public contract: kept `pub` only because integration tests
    /// under `tests/` compile against this crate's public API like any
    /// other dependent.
    #[doc(hidden)]
    pub fn setup_generation(&self) -> usize {
        self.setup_generation.load(Ordering::SeqCst)
    }

    /// Build the axum [`Router`] exposing every RisePIR HTTP endpoint over
    /// `state`. All routes are read-side (`/answer` takes the state's read
    /// lock, never the write lock) — [`Self::apply_block`] is driven
    /// directly by the caller, not through this router.
    pub fn router(state: Arc<NodeState>) -> Router {
        Self::router_with_web(state, None)
    }

    /// [`Self::router`] plus, optionally, the browser front end's static
    /// assets on the *same origin* (ADR-0019) — which is what lets the
    /// page reach `/setup`, `/answer` and `/sync` with no CORS, no mixed
    /// content, and a `connect-src 'self'` CSP. See [`crate::web`].
    pub fn router_with_web(state: Arc<NodeState>, web: Option<crate::web::WebAssets>) -> Router {
        let api = Router::new()
            .route("/answer", post(answer))
            .route("/delta/{block}", get(delta_by_block))
            .route("/sync", get(sync))
            // No concurrency cap on this route (ADR-0028 — a
            // `tower::limit::ConcurrencyLimitLayer` used to sit here,
            // `SETUP_MAX_CONCURRENT = 2`). Removed rather than tuned, for
            // two measured reasons:
            // (1) it never bounded what its doc comment claimed. tower's
            //     `ConcurrencyLimit` holds its permit in the *response
            //     future* (tower-0.5.3's `limit/concurrency/future.rs`),
            //     and this handler always returns a fully-materialized
            //     body, so the permit is released the instant the handler
            //     returns — before a single byte reaches a slow client,
            //     not after the last one does. Measured: 20 throttled
            //     readers already mid-transfer did not stop a 21st request
            //     from completing with the full body.
            // (2) the actual hazard it was meant to bound — many
            //     concurrent ~831 MB response buffers alive at once — is
            //     now structurally impossible: every `/setup` caller gets
            //     a refcounted clone of one shared, cached encode
            //     (`NodeState::setup_bytes`), so live buffers are O(1) in
            //     caller count, not O(N).
            // Bandwidth exhaustion from many legitimate-*looking* `/setup`
            // fetches is still a real concern for a public deployment;
            // that defense now belongs entirely to the reverse proxy
            // already in front of this server (Caddy, `docs/deploy.md`
            // §3.7) or a CDN (roadmap C3/C5) — a layer that can meter
            // bytes actually on the wire, which an in-process limiter on a
            // fully-materialized-body handler never could. Do not re-add a
            // cap that holds its permit across the transfer instead of
            // just the handler: that reintroduces exactly this ceiling.
            .route("/setup", get(setup))
            .route("/head", get(head))
            .route("/mode", get(mode))
            .route("/recent", get(recent))
            .route("/healthz", get(healthz))
            .layer(DefaultBodyLimit::max(MAX_ANSWER_BODY_BYTES))
            .layer(TimeoutLayer::with_status_code(StatusCode::REQUEST_TIMEOUT, REQUEST_TIMEOUT))
            .with_state(state);
        match web {
            Some(assets) => assets.attach(api),
            None => api,
        }
    }
}

// ─── handlers ──────────────────────────────────────────────────────────
//
// Every handler is panic-free on adversarial input: wire decode errors and
// query-processing errors both map to a clean `400 Bad Request` with the
// error's `Display` text, never a 500 or a panic.

/// `POST /answer?epoch=<lineage>`: decode the query bundle, answer at the
/// server's current head, encode the response bundle. Read lock only.
///
/// The epoch gate ([`epoch_gate`], ADR-0033) runs before any decode: a
/// response computed by this lineage but decoded against another
/// lineage's hint is garbage, and if the client's `pending_head` happens
/// to already equal this server's head (reachable while a re-bootstrapped
/// server replays past the client's pinned block), nothing later in the
/// client's pipeline forces the `/sync` that would catch the mismatch —
/// the garbage decodes straight to a fingerprint miss, which complete
/// mode maps to `0x0`.
async fn answer(State(state): State<Arc<NodeState>>, RawQuery(raw): RawQuery, body: Bytes) -> Response {
    if let Some(refusal) = epoch_gate(&state, raw.as_deref()) {
        return refusal;
    }
    let inner = state.inner.read().await;
    let params = inner.server.params();
    let expected_len_per_seg: Vec<u32> = state.backend_params.iter().map(|sp| sp.reshape_rows).collect();

    let queries = match wire::decode_query_bundle(&body, &params, &expected_len_per_seg) {
        Ok(q) => q,
        Err(e) => return bad_request(e),
    };

    let (responses, head) = match inner.server.answer(&queries) {
        Ok(r) => r,
        Err(e) => return bad_request(e),
    };
    drop(inner);

    octet_response(StatusCode::OK, wire::encode_response_bundle(&responses, head))
}

/// `GET /delta/{block}?epoch=<lineage>`: the immutable per-block delta,
/// cacheable forever (ADR-0006), or `404` if this block has aged out of
/// (or never entered) the retention window — or if the presented `epoch`
/// is not this deployment's lineage (missing counts as not matching).
///
/// The epoch lives in the *URL* here, deliberately, and the refusal is a
/// `404` rather than `/sync`-style `409`: this endpoint's contract is
/// `Cache-Control: immutable`, which is only sound if a given URL can
/// never serve two different byte strings. Block numbers repeat across
/// re-bootstraps with different deltas; `(epoch, block)` does not. An
/// old-lineage URL is thus permanently "no such delta" — exactly what an
/// immutable cache may remember forever (ADR-0033). Read lock only.
async fn delta_by_block(
    State(state): State<Arc<NodeState>>,
    Path(block): Path<u64>,
    RawQuery(raw): RawQuery,
) -> Response {
    if parse_epoch_param(raw.as_deref()) != Some(state.epoch.as_str()) {
        return (
            StatusCode::NOT_FOUND,
            "no delta at that (epoch, block); re-derive URLs from a fresh /setup's x-risepir-epoch",
        )
            .into_response();
    }
    let inner = state.inner.read().await;
    let plaintext_bits = inner.server.params().plaintext_bits;
    let Some(delta) = inner.per_block.get(&block).cloned() else {
        drop(inner);
        return (StatusCode::NOT_FOUND, "no delta retained for that block").into_response();
    };
    drop(inner);

    let bytes = codec::encode_block_delta(&delta, plaintext_bits);
    let mut resp = octet_response(StatusCode::OK, bytes);
    resp.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("public, max-age=31536000, immutable"));
    resp
}

/// `GET /sync?from=<u64>&to=<u64>&epoch=<lineage>`: the coalesced delta
/// for `(from, to]`, or `409 Conflict` if any part of that range has aged
/// out of the retention window **or** the presented `epoch` is not this
/// deployment's lineage (missing counts as not-this-lineage; see
/// [`epoch_gate`], ADR-0033 — a delta from one bootstrap applied to
/// another bootstrap's hint decodes to garbage, and in complete mode
/// garbage can surface as a silent `0x0`). Either way the client must
/// resync from `/setup`. Unparseable query params are rejected with
/// `400`, never treated as `0`/silently ignored. Read lock only.
async fn sync(State(state): State<Arc<NodeState>>, RawQuery(raw): RawQuery) -> Response {
    if let Some(refusal) = epoch_gate(&state, raw.as_deref()) {
        return refusal;
    }
    let Some((from, to)) = raw.as_deref().and_then(parse_sync_query) else {
        return (StatusCode::BAD_REQUEST, "expected query params ?from=<u64>&to=<u64>").into_response();
    };

    let inner = state.inner.read().await;
    let plaintext_bits = inner.server.params().plaintext_bits;
    let delta = inner.ring.range(from, to);
    drop(inner);

    match delta {
        Some(delta) => octet_response(StatusCode::OK, codec::encode_block_delta(&delta, plaintext_bits)),
        None => (
            StatusCode::CONFLICT,
            "requested range is outside the retained delta window; a full resync via /setup is required",
        )
            .into_response(),
    }
}

/// `GET /setup`: the full [`risepir_server::SetupBundle`] a fresh client
/// bootstraps from — served from a shared, refcounted cache rather than
/// re-cloned and re-encoded per request (ADR-0028; see
/// [`NodeState::setup_bytes`] for the cache/freshness mechanics).
///
/// Sets `ETag: "setup-<epoch>-<block>"`, `Cache-Control: no-cache`
/// (always revalidate, *not* "never store"), and `Accept-Ranges: bytes`
/// on every response. An `If-None-Match` that names the bundle currently
/// being served gets `304 Not Modified` with no body instead. The `304`
/// path is correct *by construction*: it is checked against the exact
/// block [`NodeState::setup_bytes`] just decided is still safe to serve,
/// which is exactly a block the ring can still bridge forward — there is
/// no second, separately-maintained "is this still valid" check to drift
/// out of sync with the one that already gates the `200` path. The
/// lineage epoch in the validator is what makes that argument hold across
/// a server *restart* too: block numbers repeat across re-bootstraps, so a
/// block-only validator could `304`-revalidate another lineage's bundle
/// whenever the heights coincided (ADR-0033); `(epoch, block)` cannot
/// collide across lineages.
///
/// Also sets `x-risepir-epoch` (the lineage token clients echo back to
/// `/sync` and `/answer` — though a client can equally re-derive it from
/// the bundle itself, [`wire::lineage_epoch`]) and `x-risepir-mode` (`"0"`
/// partial / `"1"` complete). Serving the mode *on* the setup response
/// means a client can take both from one atomic response instead of
/// racing a separate `GET /mode` against a server restart — the
/// mixed-pair window ADR-0029 accepted is closed by reading the header
/// (ADR-0033).
///
/// `If-None-Match` is attacker-controlled request input: parsed defensively
/// by [`if_none_match_hits`], never indexed blindly and never a panic on
/// malformed bytes.
///
/// # Range requests: resuming an interrupted download (ADR-0038)
///
/// A single-range `Range: bytes=<first>-[<last>]` request gets a `206
/// Partial Content` slice of the *same cached bytes* [`NodeState::setup_bytes`]
/// just decided are safe to serve — `bytes::Bytes::slice` is a refcounted
/// view, so this is O(1), never a copy of a potentially ~554 MB body.
///
/// **`If-Range` is required, and must name the *exact* `ETag` this
/// response is about to send.** This is stricter than RFC 7233 permits (a
/// bare `Range` with no `If-Range` is normally honoured unconditionally),
/// and that is deliberate, not an oversight: [`NodeState::setup_bytes`]
/// regenerates the cached bundle as the head advances (ADR-0028), and two
/// regenerations under the *same* epoch are two *different* byte strings
/// (the hint reflects every block patched in up to whichever block it is
/// pinned at, and the epoch alone does not change across a regeneration —
/// only the block does). A range computed against one regeneration's byte
/// offsets, served by mistake against a later one, would silently splice
/// two unrelated bundles into a single corrupt hint — bytes that decode to
/// garbage, which a complete-mode client can surface as a wrong `0x0`
/// (exactly the class of bug ADR-0033's epoch gate exists to prevent one
/// layer up, and the same failure mode ADR-0028's `304` validator already
/// had to fold the epoch into for the same reason). Requiring `If-Range`
/// to match the *current* `ETag` — which already encodes both the epoch
/// and the pinned block — makes a stale-regeneration range request fail
/// closed onto the always-safe full body instead of a spliced one. An
/// absent or mismatched `If-Range` therefore serves the ordinary full
/// `200`, exactly as if no `Range` header were present at all.
///
/// A well-formed range that is out of bounds (`first >= total`) is the one
/// case that *is* an error: `416 Range Not Satisfiable` with
/// `Content-Range: bytes */<total>` (RFC 7233 §4.4). Multi-range
/// (`bytes=0-1,2-3`, this server never emits a `multipart/byteranges`
/// body) and suffix-range (`bytes=-500`) requests are unsupported and, like
/// any other unparseable `Range` value, degrade to the full `200` — see
/// [`parse_range`] for the exhaustive parsing rules and
/// [`RangeOutcome`] for what each case means. Nothing here ever panics on
/// attacker-controlled `Range`/`If-Range` bytes.
///
/// axum 0.8 derives `HEAD /setup` from this same handler by stripping the
/// body afterward, so a `HEAD` request carrying a `Range` header reaches
/// every branch below exactly like a `GET` does — nothing in this function
/// is method-specific, and a test pins that a `HEAD` gets the right status
/// and headers with (as always for `HEAD`) no body.
async fn setup(State(state): State<Arc<NodeState>>, headers: HeaderMap) -> Response {
    let (bytes, block) = state.setup_bytes().await;
    let etag = format!("\"setup-{}-{block}\"", state.epoch);
    // Epoch is 16 lowercase-hex chars and `block` plain ASCII decimal
    // digits, so this can never contain a byte a `HeaderValue` rejects.
    let etag_header = HeaderValue::from_str(&etag).expect("etag: ascii hex, digits and hyphens only, always a valid header value");
    let epoch_header =
        HeaderValue::from_str(&state.epoch).expect("epoch: ascii hex only, always a valid header value");
    let mode_header = HeaderValue::from_static(if state.complete { "1" } else { "0" });

    if let Some(inm) = headers.get(header::IF_NONE_MATCH) {
        if if_none_match_hits(inm, &etag) {
            let mut resp = StatusCode::NOT_MODIFIED.into_response();
            let h = resp.headers_mut();
            h.insert(header::ETAG, etag_header);
            h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
            h.insert(HEADER_EPOCH, epoch_header);
            h.insert(HEADER_MODE, mode_header);
            return resp;
        }
    }

    let total = bytes.len() as u64;

    // Range/resume (ADR-0038) — only ever consulted once `If-Range` has
    // proven this request is talking about the *exact* bundle above, never
    // on `If-Range`'s absence or mismatch (see this fn's doc comment for
    // why that gate is mandatory rather than a nicety).
    if let Some(range_header) = headers.get(header::RANGE) {
        let if_range_matches = headers
            .get(header::IF_RANGE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v == etag);
        if if_range_matches {
            if let Ok(raw) = range_header.to_str() {
                match parse_range(raw, total) {
                    RangeOutcome::Satisfiable { first, last } => {
                        let slice = bytes.slice(first as usize..=last as usize);
                        let mut resp = octet_response(StatusCode::PARTIAL_CONTENT, slice);
                        let h = resp.headers_mut();
                        h.insert(
                            header::CONTENT_RANGE,
                            HeaderValue::from_str(&format!("bytes {first}-{last}/{total}"))
                                .expect("first/last/total: plain ASCII decimal digits, always a valid header value"),
                        );
                        h.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
                        h.insert(header::ETAG, etag_header);
                        h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
                        h.insert(HEADER_EPOCH, epoch_header);
                        h.insert(HEADER_MODE, mode_header);
                        return resp;
                    }
                    RangeOutcome::Unsatisfiable => {
                        let mut resp = StatusCode::RANGE_NOT_SATISFIABLE.into_response();
                        let h = resp.headers_mut();
                        h.insert(
                            header::CONTENT_RANGE,
                            HeaderValue::from_str(&format!("bytes */{total}"))
                                .expect("total: plain ASCII decimal digits, always a valid header value"),
                        );
                        h.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
                        return resp;
                    }
                    // Multi-range, a suffix range, or anything else this
                    // parser does not recognise: fall through to the full
                    // `200` below, exactly as if `Range` were absent.
                    RangeOutcome::Unsupported => {}
                }
            }
            // A `Range` value that fails to decode as UTF-8 also falls
            // through to the full `200` below — never a panic.
        }
    }

    let mut resp = octet_response(StatusCode::OK, bytes);
    let h = resp.headers_mut();
    h.insert(header::ETAG, etag_header);
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    h.insert(HEADER_EPOCH, epoch_header);
    h.insert(HEADER_MODE, mode_header);
    h.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    resp
}

/// `GET /head`: the server's current block, as an 8-byte little-endian
/// `u64` body, plus `x-risepir-epoch` and `x-risepir-mode` headers — a
/// cheap way for a long-lived client to notice a lineage change on its
/// next poll instead of on its next `/sync` `409` (informational;
/// `/sync`/`/answer` remain the enforcement points, ADR-0033).
///
/// `x-risepir-mode` is new here (ADR-0038): a persistent hint cache needs
/// the *(epoch, mode)* pair before it can even decide whether a cached
/// bundle applies, and asking for it here is far cheaper than a `/setup`
/// request, which can pay `NodeState::setup_bytes`'s cache-regeneration
/// cost (~10 s CPU at the complete set). Both headers are read from the
/// same `NodeState` `/setup` already reads them from, so the pair still
/// cannot straddle a restart the way two *separate* requests could — this
/// preserves exactly the property ADR-0033 introduced the header for in
/// the first place (there, putting mode *on* `/setup`'s own response
/// closed the race between a freestanding `GET /mode` and a concurrent
/// re-bootstrap; here, the same atomicity argument applies to `/head`
/// instead of `/setup`, and for the same reason). Read lock only.
async fn head(State(state): State<Arc<NodeState>>) -> Response {
    let inner = state.inner.read().await;
    let h = inner.server.block();
    drop(inner);
    let mut resp = octet_response(StatusCode::OK, h.to_le_bytes().to_vec());
    let hdrs = resp.headers_mut();
    hdrs.insert(
        HEADER_EPOCH,
        HeaderValue::from_str(state.epoch()).expect("epoch: ascii hex only, always a valid header value"),
    );
    hdrs.insert(HEADER_MODE, HeaderValue::from_static(if state.complete { "1" } else { "0" }));
    resp
}

/// `GET /mode`: one byte — `1` if this deployment serves the *complete*
/// nonzero-balance set, `0` if partial. A remote front end keys its
/// `NotFound` policy on this (never guessed; see [`NodeState::new`]).
/// No lock needed: the flag is fixed at construction.
async fn mode(State(state): State<Arc<NodeState>>) -> Response {
    octet_response(StatusCode::OK, vec![u8::from(state.complete)])
}

/// `GET /recent`: up to [`RECENT_CAPACITY`] recently-touched addresses,
/// newest first, as `count:u32-le ‖ count × 20 bytes` — the same binary
/// discipline as every other response here, and trivially parsed by the
/// front end.
///
/// Public chain data, identical for every caller, and separate from any
/// query: see [`NodeState`]'s `recent` field for why serving it does not
/// weaken the PIR property.
async fn recent(State(state): State<Arc<NodeState>>) -> Response {
    let recent = state.recent.read().await;
    let mut body = Vec::with_capacity(4 + recent.len() * 20);
    body.extend_from_slice(&(recent.len() as u32).to_le_bytes());
    for addr in recent.iter().rev() {
        body.extend_from_slice(addr);
    }
    drop(recent);
    let mut resp = octet_response(StatusCode::OK, body);
    resp.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    resp
}

/// `GET /healthz`: liveness + readiness, plus the reconcile check's own
/// health (ADR-0027), as plain text — `200` with one value/line per line:
///
/// ```text
/// ok <head-block>
/// reconcile_configured=<0|1>
/// reconcile_last_checkpoint_block=<u64>
/// reconcile_last_checkpoint_unix=<u64>
/// reconcile_last_success_block=<u64>
/// reconcile_last_success_unix=<u64>
/// reconcile_comparisons_total=<u64>
/// reconcile_checkpoints_total=<u64>
/// reconcile_consecutive_dark=<u64>
/// reconcile_halted=<0|1>
/// reconcile_deferred_total=<u64>
/// reconcile_reservoir_checks_total=<u64>
/// reconcile_reservoir_len=<u64>
/// ```
///
/// e.g. `ok 19123456` followed by twelve `reconcile_*=…` lines. The last
/// three (`deferred_total`/`reservoir_checks_total`/`reservoir_len`) are an
/// ADR-0036 addition, appended after `reconcile_halted` — every existing
/// line above them is unchanged, in the same order.
///
/// # Compatibility guarantee
///
/// The **first line is, byte for byte, exactly what it always was:
/// `ok <head-block>` and nothing else on that line.** Any existing monitor
/// that checks a status-line prefix or reads only the first line keeps
/// working unmodified. Every `reconcile_*` line is a pure addition below
/// it, one `key=value` pair per line, in the fixed order above. A field is
/// always present, never omitted — a deployment that does not reconcile at
/// all (mock/demo, or `mainnet --reconcile-every 0`) still emits every
/// line, with `reconcile_configured=0`: an *absent* field would read as
/// "healthy" to a monitor that does not know better, which is precisely
/// the blind spot this endpoint exists to close. This stays plain text
/// (not JSON) deliberately — `/healthz` was already this crate's one
/// text-format exception to the binary-everywhere-else rule (`docs/plan.md`
/// ADR-0006), and a handful of counters does not earn a second format.
///
/// Taking the read lock for the head number is deliberate: a server wedged
/// behind a stuck writer fails this probe via the request timeout instead
/// of reporting healthy while unable to answer. For monitors, a stalling
/// head number is the block-lag signal (roadmap C7); the reconcile fields
/// are the integrity-check-lag signal — anything beyond that belongs to
/// real metrics, not a health probe.
async fn healthz(State(state): State<Arc<NodeState>>) -> Response {
    let inner = state.inner.read().await;
    let h = inner.server.block();
    drop(inner);
    let rh = state.reconcile_health();
    let body = format!(
        "ok {h}\n\
         reconcile_configured={}\n\
         reconcile_last_checkpoint_block={}\n\
         reconcile_last_checkpoint_unix={}\n\
         reconcile_last_success_block={}\n\
         reconcile_last_success_unix={}\n\
         reconcile_comparisons_total={}\n\
         reconcile_checkpoints_total={}\n\
         reconcile_consecutive_dark={}\n\
         reconcile_halted={}\n\
         reconcile_deferred_total={}\n\
         reconcile_reservoir_checks_total={}\n\
         reconcile_reservoir_len={}\n",
        u8::from(rh.configured),
        rh.last_checkpoint_block,
        rh.last_checkpoint_unix,
        rh.last_success_block,
        rh.last_success_unix,
        rh.comparisons_total,
        rh.checkpoints_total,
        rh.consecutive_dark,
        u8::from(rh.halted),
        rh.deferred_total,
        rh.reservoir_checks_total,
        rh.reservoir_len,
    );
    (StatusCode::OK, body).into_response()
}

// ─── small helpers ────────────────────────────────────────────────────

/// Builds a `200`-or-caller-chosen-`status` response with an explicit
/// `application/octet-stream` content type — every successful response
/// this crate returns is raw binary, never text or JSON. Generic over
/// anything axum already knows how to turn into a body: `Vec<u8>` for most
/// handlers, [`Bytes`] for `/setup` (ADR-0028) — so the cached, refcounted
/// bytes reach the client without an extra copy back into an owned `Vec`.
fn octet_response(status: StatusCode, body: impl IntoResponse) -> Response {
    let mut resp = (status, body).into_response();
    resp.headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static("application/octet-stream"));
    resp
}

/// Whether the (attacker-controlled) raw `If-None-Match` header value `raw`
/// matches `etag` — the standard comma-separated-list / `*` wildcard forms
/// (RFC 9110 §13.1.2), tolerating an optional leading weak-validator `W/`
/// prefix on any listed tag (this server only ever emits strong ETags
/// itself, but a client or intermediary echoing one back with `W/`
/// prepended should still be recognised rather than silently miss the
/// match and pay for a full re-download).
///
/// Never panics or indexes blindly: a header value that is not valid UTF-8
/// (`to_str` failing) simply does not match — malformed input degrades to
/// "serve the full `200` body", the always-safe default, never a crash.
fn if_none_match_hits(raw: &HeaderValue, etag: &str) -> bool {
    let Ok(raw) = raw.to_str() else { return false };
    raw.split(',').any(|tok| {
        let tok = tok.trim();
        tok == "*" || tok == etag || tok.strip_prefix("W/").map(str::trim) == Some(etag)
    })
}

/// `400 Bad Request` with `err`'s `Display` text as the body — the uniform
/// mapping every decode/processing error in this crate's handlers goes
/// through, so a caller always gets a legible reason rather than an opaque
/// status code.
fn bad_request(err: impl std::fmt::Display) -> Response {
    (StatusCode::BAD_REQUEST, err.to_string()).into_response()
}

/// Extracts `epoch=<value>` out of a raw query string (order-independent,
/// other params ignored). `None` when the param is absent — the caller
/// decides what absence means; it is never defaulted.
fn parse_epoch_param(raw: Option<&str>) -> Option<&str> {
    raw?.split('&').find_map(|pair| {
        let (key, val) = pair.split_once('=')?;
        (key == "epoch").then_some(val)
    })
}

/// The lineage gate (ADR-0033) shared by `/sync` and `/answer`: `None`
/// when the request's `epoch` query param matches this deployment's, or
/// `Some(409)` with a distinct message for a missing vs a mismatched
/// value. Both cases mean the same thing to a client — the bundle it
/// bootstrapped from is not the lineage this server serves (or it never
/// learned one), and the only sound recovery is a fresh `/setup` — which
/// is exactly what every client already does on `409` (the CLI
/// re-bootstraps, ADR-0029; the browser surfaces "reload the page").
///
/// The comparison is over attacker-controlled input, but both sides are
/// plain `str`s and the epoch is not a secret (it is served openly in
/// `/setup`'s `ETag` and `x-risepir-epoch`) — nothing here needs
/// constant-time discipline.
fn epoch_gate(state: &NodeState, raw_query: Option<&str>) -> Option<Response> {
    match parse_epoch_param(raw_query) {
        Some(presented) if presented == state.epoch => None,
        Some(_) => Some(
            (
                StatusCode::CONFLICT,
                "epoch mismatch: this server was re-bootstrapped onto a different hint lineage \
                 since your /setup; a full resync via /setup is required",
            )
                .into_response(),
        ),
        None => Some(
            (
                StatusCode::CONFLICT,
                "missing epoch: this endpoint requires the ?epoch= lineage token from /setup \
                 (x-risepir-epoch); a full resync via /setup is required",
            )
                .into_response(),
        ),
    }
}

/// What [`parse_range`] decided about a `Range` request header, against a
/// resource of some known `total` byte length.
///
/// Only a single `bytes=<first>-[<last>]` range is ever honoured — see
/// [`setup`]'s doc comment for why a mandatory, matching `If-Range` gates
/// this parser being consulted at all, and this type's own variants for
/// what everything else degrades to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RangeOutcome {
    /// No usable single-range request: a suffix range (`bytes=-500`), a
    /// multi-range request (`bytes=0-1,2-3`), or anything that fails to
    /// parse as `bytes=<u64>-[<u64>]` (including an integer too large for
    /// `u64`, or a `last-byte-pos` before `first-byte-pos`). The caller
    /// must serve the ordinary full response, exactly as if `Range` were
    /// absent — never a panic, never a guess at what the client meant.
    Unsupported,
    /// `first-byte-pos` is at or past `total` — RFC 7233 §4.4: not
    /// satisfiable regardless of `last-byte-pos`. The caller answers `416`.
    Unsatisfiable,
    /// A satisfiable, inclusive byte range within `0..=total-1`. `last`
    /// already has an absent or over-long `last-byte-pos` clamped down to
    /// `total - 1` (RFC 7233 §2.1: "the byte range is interpreted as the
    /// remainder of the representation"), so the caller can slice
    /// `first..=last` directly with no further bounds-checking.
    Satisfiable {
        /// First byte of the range, inclusive; always `< total`.
        first: u64,
        /// Last byte of the range, inclusive; always `>= first` and
        /// `< total`.
        last: u64,
    },
}

/// Parses a `Range: bytes=<first>-[<last>]` request header (RFC 7233 §2.1)
/// against a resource of `total` bytes. Never panics on any input —
/// `Range` is attacker-controlled request data like every other header
/// this crate reads off the wire, and a parser this small earns exhaustive
/// unit tests rather than a general-purpose crate dependency: this file's
/// other hand-rolled header/query parsers ([`if_none_match_hits`],
/// [`parse_epoch_param`], [`parse_sync_query`]) all take the same
/// approach.
///
/// Deliberately narrower than the RFC allows, in two ways, both of which
/// degrade to [`RangeOutcome::Unsupported`] rather than an error: a
/// suffix-length range (`bytes=-500`, "the last N bytes") is not
/// implemented, and a multi-range request (`bytes=0-1,2-3`) is not
/// implemented (this server never emits a `multipart/byteranges` body).
/// Both are safe to leave unsupported because the caller's fallback is
/// simply the ordinary full `200` — a client that asked for a range it
/// cannot get here still gets a correct, complete bundle, just not a
/// partial one.
fn parse_range(raw: &str, total: u64) -> RangeOutcome {
    let Some(spec) = raw.strip_prefix("bytes=") else {
        return RangeOutcome::Unsupported;
    };
    if spec.contains(',') {
        return RangeOutcome::Unsupported;
    }
    let Some((first_s, last_s)) = spec.split_once('-') else {
        return RangeOutcome::Unsupported;
    };
    if first_s.is_empty() {
        // A suffix range (`bytes=-500`) — unsupported, see the doc comment.
        return RangeOutcome::Unsupported;
    }
    let Ok(first) = first_s.parse::<u64>() else {
        return RangeOutcome::Unsupported;
    };
    if first >= total {
        return RangeOutcome::Unsatisfiable;
    }
    let last = if last_s.is_empty() {
        total - 1
    } else {
        match last_s.parse::<u64>() {
            // An over-long `last-byte-pos` folds down to the end of the
            // representation (RFC 7233 §2.1) rather than being rejected.
            Ok(l) if l >= first => l.min(total - 1),
            // `last < first` is a syntactically invalid byte-range-spec
            // (RFC 7233 §2.1: "MUST be ignored"); anything that fails to
            // parse as a plain `u64` (including a value too large to fit
            // one) is simply not a range this parser understands. Both
            // degrade to the same safe fallback as every other malformed
            // case here.
            _ => return RangeOutcome::Unsupported,
        }
    };
    RangeOutcome::Satisfiable { first, last }
}

/// Parses `from=<u64>&to=<u64>` (order-independent, extra/unknown params
/// ignored) out of a raw query string. `None` for anything that doesn't
/// cleanly parse both — the caller maps that to `400`, never guessing a
/// default.
fn parse_sync_query(q: &str) -> Option<(u64, u64)> {
    let mut from: Option<u64> = None;
    let mut to: Option<u64> = None;
    for pair in q.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (key, val) = pair.split_once('=')?;
        match key {
            "from" => from = val.parse::<u64>().ok(),
            "to" => to = val.parse::<u64>().ok(),
            _ => {}
        }
    }
    match (from, to) {
        (Some(f), Some(t)) => Some((f, t)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_sync_query_accepts_both_orders() {
        assert_eq!(parse_sync_query("from=1&to=5"), Some((1, 5)));
        assert_eq!(parse_sync_query("to=5&from=1"), Some((1, 5)));
    }

    #[test]
    fn parse_sync_query_ignores_unknown_params() {
        assert_eq!(parse_sync_query("foo=bar&from=1&to=5&baz=qux"), Some((1, 5)));
    }

    #[test]
    fn parse_sync_query_rejects_missing_or_malformed() {
        assert_eq!(parse_sync_query(""), None);
        assert_eq!(parse_sync_query("from=1"), None);
        assert_eq!(parse_sync_query("to=5"), None);
        assert_eq!(parse_sync_query("from=abc&to=5"), None);
        assert_eq!(parse_sync_query("from=1&to=-5"), None);
        assert_eq!(parse_sync_query("garbage"), None);
    }

    #[test]
    fn parse_range_open_ended_reaches_the_true_end() {
        assert_eq!(parse_range("bytes=0-", 1000), RangeOutcome::Satisfiable { first: 0, last: 999 });
        assert_eq!(parse_range("bytes=500-", 1000), RangeOutcome::Satisfiable { first: 500, last: 999 });
    }

    #[test]
    fn parse_range_explicit_bounds() {
        assert_eq!(parse_range("bytes=100-199", 1000), RangeOutcome::Satisfiable { first: 100, last: 199 });
        assert_eq!(parse_range("bytes=0-0", 1000), RangeOutcome::Satisfiable { first: 0, last: 0 }, "a single byte");
    }

    #[test]
    fn parse_range_over_long_last_clamps_to_total_minus_one() {
        // RFC 7233 §2.1: an over-long last-byte-pos folds down to the end
        // of the representation rather than being rejected.
        assert_eq!(
            parse_range("bytes=100-999999999999", 1000),
            RangeOutcome::Satisfiable { first: 100, last: 999 }
        );
    }

    #[test]
    fn parse_range_suffix_is_unsupported() {
        // "last N bytes" — deliberately not implemented; see the doc
        // comment. Must degrade to the safe fallback, never guess N.
        assert_eq!(parse_range("bytes=-500", 1000), RangeOutcome::Unsupported);
        assert_eq!(parse_range("bytes=-0", 1000), RangeOutcome::Unsupported);
    }

    #[test]
    fn parse_range_malformed_is_unsupported_never_panics() {
        assert_eq!(parse_range("bytes=abc", 1000), RangeOutcome::Unsupported);
        assert_eq!(parse_range("bytes=", 1000), RangeOutcome::Unsupported);
        assert_eq!(parse_range("", 1000), RangeOutcome::Unsupported);
        assert_eq!(parse_range("garbage", 1000), RangeOutcome::Unsupported);
        assert_eq!(parse_range("bytes=100", 1000), RangeOutcome::Unsupported, "no hyphen at all");
        assert_eq!(parse_range("Bytes=0-9", 1000), RangeOutcome::Unsupported, "unit token is case-sensitive here");
    }

    #[test]
    fn parse_range_last_before_first_is_unsupported() {
        assert_eq!(parse_range("bytes=5-1", 1000), RangeOutcome::Unsupported);
    }

    #[test]
    fn parse_range_multi_range_is_unsupported() {
        assert_eq!(parse_range("bytes=0-99,200-299", 1000), RangeOutcome::Unsupported);
        assert_eq!(parse_range("bytes=0-,", 1000), RangeOutcome::Unsupported, "a trailing comma is still multi-range shaped");
    }

    #[test]
    fn parse_range_huge_numbers_never_panic_or_wrap() {
        // Larger than u64::MAX by a wide margin, on either side of the
        // hyphen — must not panic and must not silently wrap.
        assert_eq!(parse_range("bytes=99999999999999999999999999-", 1000), RangeOutcome::Unsupported);
        assert_eq!(parse_range("bytes=0-99999999999999999999999999", 1000), RangeOutcome::Unsupported);
        assert_eq!(
            parse_range("bytes=99999999999999999999999999-99999999999999999999999999", 1000),
            RangeOutcome::Unsupported
        );
    }

    #[test]
    fn parse_range_first_at_or_past_total_is_unsatisfiable() {
        assert_eq!(parse_range("bytes=1000-", 1000), RangeOutcome::Unsatisfiable, "first == total");
        assert_eq!(parse_range("bytes=5000-", 1000), RangeOutcome::Unsatisfiable, "first > total");
        assert_eq!(parse_range("bytes=0-", 0), RangeOutcome::Unsatisfiable, "an empty resource is never satisfiable");
    }

    #[test]
    fn parse_epoch_param_extracts_only_the_epoch_key() {
        assert_eq!(parse_epoch_param(Some("from=1&to=5&epoch=00ff00ff00ff00ff")), Some("00ff00ff00ff00ff"));
        assert_eq!(parse_epoch_param(Some("epoch=abc&from=1")), Some("abc"));
        assert_eq!(parse_epoch_param(Some("epoch=")), Some(""), "an empty value is presented, not absent — it will simply mismatch");
        assert_eq!(parse_epoch_param(Some("from=1&to=5")), None);
        assert_eq!(parse_epoch_param(Some("EPOCH=abc")), None, "case-sensitive, like every other param here");
        assert_eq!(parse_epoch_param(Some("garbage")), None);
        assert_eq!(parse_epoch_param(None), None);
    }
}
