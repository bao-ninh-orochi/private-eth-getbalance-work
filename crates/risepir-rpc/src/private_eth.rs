//! [`PrivateEth`] — the PIR-backed private-`eth_getBalance` core: one
//! `Mutex`-guarded [`Session`] talking to one [`PirHttpClient`]
//! (`docs/plan.md` §3.3/§3.6, ADR-0003, ADR-0006, ADR-0010).

use std::time::{Duration, Instant};

use ikpir_common::SimplePirBackend;
use risepir_client::{FinishTimings, Lookup, RisePirClient};
use risepir_http::PirHttpClient;
use risepir_proto::{keccak256, AddressHash, ValueCodec};
use risepir_server::SetupBundle;

use crate::error::RpcError;

/// One completed delta fetch — the unit the blocks side of a
/// measurement is reported in.
///
/// Exactly one of these is produced per `GET /sync` the session
/// performs, wherever that fetch was issued from: the catch-up inside a
/// balance lookup (twice per lookup at most) or a standalone
/// `PrivateEth::follow_once`. A caller that writes one row per fetch
/// therefore has complete coverage with no double-counting — which is
/// what makes the per-block delta series continuous rather than gapped
/// at every trial.
///
/// The wire, decode and byte fields are attributed to *this* fetch by
/// subtracting a [`risepir_http::NetSink`] snapshot taken either side of
/// the call; they are zero when the transport is uninstrumented.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SyncFetch {
    /// The accumulator's head before the fetch (the `from` of `(from, to]`).
    pub from_block: u64,
    /// The accumulator's head after it (the `to`).
    pub to_block: u64,
    /// `to_block - from_block`: how many blocks this one fetch covered.
    /// `/sync` serves a **coalesced** delta for the whole range, so a
    /// value above 1 means every cost here is a range total, not one
    /// block's.
    pub blocks: u64,
    /// Wall-clock arrival, milliseconds since the Unix epoch, taken once
    /// the delta has been ingested. `0` when uninstrumented.
    pub received_at_unix_ms: u128,
    /// Wire time for this fetch: before send → last body byte.
    pub wire: Duration,
    /// Decoding this fetch's bytes into a `BlockDelta`.
    pub decode: Duration,
    /// This fetch's `/sync` response body length.
    pub wire_bytes: u64,
    /// [`RisePirClient::ingest_delta`] — folding the fetched delta into
    /// the rolling `ΔD`.
    pub ingest: Duration,
    /// Nonzero `(segment, row, offset)` cells carried by the fetched
    /// delta itself.
    pub cells_in_delta: usize,
    /// `|ΔD|` — the client's total pending delta cells *after* the
    /// ingest.
    pub delta_cells_total: usize,
}

/// Client-side timers for one [`PrivateEth::get_balance`] call.
///
/// Every field is a duration or a block/cell count. Nothing here is
/// derived from the address or the balance — see [`Self::found`], the
/// single bit of answer-shaped information recorded, which is what makes
/// this safe to write to a log or a CSV.
///
/// # What is *not* here, and why
///
/// The query path's own network time. Each call's wire span is recorded
/// by the transport ([`risepir_http::NetSink`]), timed from just before
/// the request is sent to the last body byte. Re-timing it here would
/// double-count it and would fold the wire codec into "network". A
/// caller adds the two sources; nothing overlaps. (The exception is
/// [`Self::sync`], where each fetch's own wire/decode/bytes *are*
/// carried, because attributing them to an individual fetch needs a
/// snapshot taken around that one call and only this layer knows where
/// the call boundaries are.)
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BalanceTimings {
    /// [`RisePirClient::build_query`] — the LWE encryption of the query
    /// bundle, all segments.
    pub build: Duration,
    /// [`RisePirClient::finish`] end to end — the whole rewind
    /// (`docs/plan.md` §3.3 steps 2-5).
    pub finish: Duration,
    /// [`Self::finish`] broken into ADR-0003's four steps. Their sum is
    /// slightly under [`Self::finish`]; the difference is the argument
    /// checks, the `candidate_buckets` re-hash, and the final value
    /// decode, which sit outside the per-segment loop.
    pub finish_parts: FinishTimings,
    /// Every delta fetch this call performed, in order — one entry per
    /// `GET /sync`, so a caller can write one blocks-CSV row per fetch
    /// with no gaps and no double-counting. Empty (the common case) for
    /// a caller that already follows head between queries; at most two
    /// entries per attempt (the pre-answer catch-up and the one to the
    /// answered block).
    ///
    /// This vec is reset once, at the start of the call, not once per
    /// attempt (unlike every other field here — see
    /// [`BalanceTimings::attempts`]): on an `attempts = 2` row it holds
    /// whatever the failed first attempt already pushed *plus* the
    /// retry's own fetches, up to four entries spanning both attempts.
    /// That accumulation is deliberate, the same reason a single
    /// attempt's two fetches both get kept — it is what keeps the
    /// per-block delta series continuous across a rebootstrap-and-retry
    /// instead of gapped at it.
    pub sync: Vec<SyncFetch>,
    /// The block the server answered at.
    pub at_block: u64,
    /// The block the client's *hint* is pinned at
    /// ([`RisePirClient::pinned_block`]). Constant for the lifetime of a
    /// session that never garbage-collects, which is the product's own
    /// behaviour — so `at_block - pinned_block` is the hint's staleness.
    pub pinned_block: u64,
    /// `|ΔD|` at the moment the query was answered.
    pub delta_cells: usize,
    /// Whether the scan found a matching entry. The *only* bit of
    /// answer-derived information recorded: `NotFound` in complete mode
    /// is a legitimate `0x0`, and distinguishing it from a hit is
    /// exactly what a not-found-path measurement needs. The balance
    /// itself never enters this struct.
    pub found: bool,
    /// How many attempts the call took: `1` normally, `2` when a
    /// `Stalled` forced one re-bootstrap-and-retry. Every field here
    /// except [`BalanceTimings::sync`] records only the last attempt's
    /// timers: on a `2` row, `build`, `finish`, `finish_parts`,
    /// `at_block`, `pinned_block`, `delta_cells`, and `found` are the
    /// retry's values alone, so they still do not account for the whole
    /// wall time. `sync` is the exception — it accumulates across both
    /// attempts rather than being reset for the retry; see its own docs.
    pub attempts: u32,
}

/// Minimum spacing between two re-bootstrap attempts (ADR-0029, amended).
/// A re-bootstrap is a full `/setup` re-download — 553.82 MB at the live
/// complete set's deployed `(arity 2, bucket_size 4)` geometry (ADR-0034;
/// was 830.73 MB at the previous `(arity 3, bucket_size 4)` geometry,
/// ~8 minutes measured there and proportionally less now, not re-measured
/// at the new size) — so when a catch-up-replaying server outruns even a
/// freshly bootstrapped client (ADR-0029's own motivating case), retrying
/// per *call* turns a polling caller into an unmetered re-download loop:
/// every lookup pays the download and still stalls. Within this window of
/// the previous attempt, [`PrivateEth::get_balance`] reports the stall
/// honestly instead of paying again — erroring is fine, a 554 MB-per-call
/// retry loop is not. Five minutes sits between the (pre-ADR-0034)
/// ~8-minute worst-case download (retrying faster than one download can
/// even finish is provably useless) and the ~10-minute window a freshly
/// regenerated `/setup` bundle now leaves a client even at replay speed
/// (`NodeState::setup_bytes`'s freshness rule).
pub(crate) const REBOOTSTRAP_COOLDOWN: std::time::Duration = std::time::Duration::from_secs(300);

/// Everything about one bootstrapped RisePIR session that must move
/// together, atomically, whenever the client re-bootstraps
/// ([`PrivateEth::rebootstrap`]): the rewind client itself, the block its
/// pending delta accumulator currently covers up to, and every piece of
/// this deployment's geometry / `NotFound` policy the client was
/// bootstrapped against.
///
/// # Why `pending_head` exists alongside the client, not inside it
///
/// [`RisePirClient::pinned_block`] is the block the client's *hint* is
/// pinned at — it only moves on an explicit `collect_garbage` (never
/// called by this crate; see [`PrivateEth::get_balance`]'s docs). What
/// this type actually needs to track is the block its **pending delta
/// accumulator** currently covers up to: [`RisePirClient::ingest_delta`]
/// requires each new delta to extend that accumulator strictly forward,
/// and [`RisePirClient::finish`] requires the response's `at_block` to
/// equal it *exactly* (`docs/plan.md` §3.3 step 2 — "the response names
/// the epoch," ADR-0006). `RisePirClient` does not expose that head as a
/// public accessor (deliberately — it is accumulator-internal
/// bookkeeping, not part of the rewind's public contract), so this type
/// tracks its own copy, updated in lockstep with the client under the
/// same lock (see [`PrivateEth::sync_to`]).
///
/// # Why one struct, not five separate fields
///
/// Every field here was derived from the *same* [`SetupBundle`] /
/// `complete` pair at some point in time (see [`Self::from_bundle`]).
/// Grouping them means [`PrivateEth::rebootstrap`] can replace the
/// entire session in one assignment (`*session = ...`) — never
/// field-by-field — which is what rules out a mixed state (a new hint
/// paired with a stale `strict_not_found`, or vice versa) that
/// `docs/plan.md`'s "never return a wrong answer" invariant forbids.
pub(crate) struct Session {
    client: RisePirClient<SimplePirBackend>,
    /// See this struct's "Why `pending_head` exists alongside the
    /// client" docs above. Invariant: only ever read or written while
    /// [`PrivateEth`]'s session lock is held (it is, for the entirety of
    /// every method that takes `&mut Session`).
    pending_head: u64,
    arity: usize,
    plaintext_bits: u32,
    /// This deployment's per-segment `reshape_row_width` — geometry
    /// [`PirHttpClient::answer`] needs to decode a response bundle but
    /// does not itself store (`risepir-http`'s `PirHttpClient` docs).
    reshape_row_width_per_seg: Vec<u32>,
    /// `NotFound` policy. `false` (complete nonzero set, ADR-0015):
    /// absence ⟺ zero, answer `0x0`. `true` (partial deployment — no
    /// complete snapshot; only accounts touched since bootstrap are
    /// tracked): absence means *unknown*, and answering `0x0` for an
    /// account that merely predates the bootstrap would be a wrong
    /// answer, so `NotFound` becomes [`RpcError::NotInTrackedSet`]
    /// instead. Erroring is fine; a silently wrong `0x0` is not.
    strict_not_found: bool,
    /// The lineage token of the bundle this session bootstrapped from
    /// ([`risepir_http::wire::lineage_epoch`], ADR-0033), echoed to
    /// `/sync` and `/answer` so the server refuses (409) to serve this
    /// session anything from a *different* bootstrap's lineage — deltas
    /// or answers that would decode to garbage against this hint, which
    /// complete mode could surface as a silent `0x0`.
    epoch: String,
}

impl Session {
    /// Derive a fresh session from a freshly fetched [`SetupBundle`] and
    /// this deployment's freshly fetched `complete` mode (`GET /mode`) —
    /// the one place every geometry field is derived (never hardcoded;
    /// `docs/plan.md`'s invariant) and `strict_not_found` is set (never
    /// carried over from any prior session). Used by both
    /// [`PrivateEth::from_setup`] (first bootstrap) and
    /// [`PrivateEth::rebootstrap`] (after a stall) — the latter is
    /// exactly the former, run again, which is what makes a re-bootstrap
    /// introduce no new trust or correctness surface.
    fn from_bundle(
        bundle: SetupBundle<SimplePirBackend>,
        value_codec: ValueCodec,
        complete: bool,
    ) -> Self {
        let arity = bundle.params.arity();
        let plaintext_bits = bundle.params.plaintext_bits;
        let reshape_row_width_per_seg: Vec<u32> = bundle
            .backend_params
            .iter()
            .map(|sp| sp.reshape_row_width)
            .collect();
        let epoch = risepir_http::wire::lineage_epoch(&bundle.backend_params);
        let pending_head = bundle.block;
        let client = RisePirClient::from_setup(bundle, value_codec);
        Self {
            client,
            pending_head,
            arity,
            plaintext_bits,
            reshape_row_width_per_seg,
            strict_not_found: !complete,
            epoch,
        }
    }
}

/// The private `eth_getBalance` core for one RisePIR deployment.
///
/// # Concurrency (ADR-0010)
///
/// One shared `Mutex<Session>`; [`Self::get_balance`] holds the lock for
/// its entire body, including the network round trips to `self.pir` —
/// never just around the in-memory client calls. This is required, not
/// merely convenient: [`RisePirClient::build_query`] documents a
/// single-in-flight-query-per-segment contract (a second `build_query`
/// before the first's matching `finish` discards the first query's
/// secret), so two `get_balance` calls interleaving their
/// `build_query`/`answer`/`finish` steps would corrupt each other's
/// lookups.
///
/// Before this type existed, `pending_head` was a separate `AtomicU64`
/// sitting beside a `Mutex<RisePirClient>` — correct only because of an
/// easy-to-violate *external* invariant ("only ever touched while the
/// client's lock is held"), documented in a comment rather than enforced
/// by the type system. Folding it into `Session`, one mutex-guarded
/// struct alongside the client and the geometry it was derived from,
/// removes that invariant instead of merely documenting it: there is no
/// longer a separate atomic that *could* be read or written out from
/// under the lock, by construction. It is also what makes
/// `rebootstrap` sound — see `Session`'s own docs for why a
/// single struct, not five fields, is what makes that swap atomic.
pub struct PrivateEth {
    pub(crate) session: tokio::sync::Mutex<Session>,
    pub(crate) pir: PirHttpClient,
    /// When the last re-bootstrap *started*, if any — the cooldown clock
    /// for [`Self::get_balance`]'s stall recovery (ADR-0029, amended):
    /// one re-bootstrap costs a full `/setup` re-download (553.82 MB at
    /// the live complete set's deployed `(arity 2, bucket_size 4)`
    /// geometry, ADR-0034; was 830.73 MB pre-ADR-0034), so when a
    /// replaying server outruns even a fresh bootstrap, per-call retries
    /// would turn a polling caller
    /// into an unmetered re-download loop. Within
    /// [`REBOOTSTRAP_COOLDOWN`] of the previous attempt, a stalled call
    /// reports the stall honestly instead of paying again. A `std`
    /// mutex, never held across an `.await` (tokio's own guidance for
    /// few-field critical sections); always locked while the `session`
    /// tokio mutex is already held, so there is no lock-order ambiguity.
    pub(crate) last_rebootstrap: std::sync::Mutex<Option<std::time::Instant>>,
    /// This deployment's `ValueCodec` — fixed workspace-wide, not a
    /// per-request choice, but stored (rather than hardcoded inline)
    /// so [`Self::rebootstrap`] can rebuild a [`RisePirClient`] from a
    /// freshly fetched [`SetupBundle`] without reaching back into any
    /// particular construction site's own copy of it.
    pub(crate) value_codec: ValueCodec,
    pub(crate) chain_id: u64,
    pub(crate) proxy_upstream: Option<String>,
    /// A plain HTTP client for forwarding a denied method's raw body to
    /// `proxy_upstream` verbatim (ADR-0012) — deliberately a fresh
    /// [`reqwest::Client`] rather than reusing `pir`'s internal one
    /// ([`PirHttpClient`] talks only to the local PIR server; conflating
    /// "the trusted local PIR transport" with "an arbitrary external
    /// upstream the operator opted into leaking to" would blur a
    /// deliberately drawn trust boundary for no real gain — connection
    /// pooling across two logically distinct upstreams is not worth that).
    pub(crate) proxy_http: reqwest::Client,
}

impl PrivateEth {
    /// Bootstrap a [`PrivateEth`] from a freshly fetched [`SetupBundle`]
    /// and this deployment's freshly fetched `complete` mode (`GET
    /// /mode`) — the single place every deployment geometry field and
    /// the `NotFound` policy are derived, shared by every construction
    /// site (`front.rs`, `mainnet.rs`, `demo.rs`) and by
    /// `rebootstrap`, so there is exactly one implementation of
    /// "how to turn a `SetupBundle` into a session" to keep correct.
    ///
    /// `value_codec` and `chain_id` are this deployment's fixed
    /// configuration; `proxy_upstream` is the opt-in ADR-0012 leak path,
    /// if any.
    pub fn from_setup(
        pir: PirHttpClient,
        bundle: SetupBundle<SimplePirBackend>,
        value_codec: ValueCodec,
        complete: bool,
        chain_id: u64,
        proxy_upstream: Option<String>,
    ) -> Self {
        Self {
            session: tokio::sync::Mutex::new(Session::from_bundle(bundle, value_codec, complete)),
            pir,
            last_rebootstrap: std::sync::Mutex::new(None),
            value_codec,
            chain_id,
            proxy_upstream,
            proxy_http: reqwest::Client::new(),
        }
    }

    /// This deployment's configured chain id (`eth_chainId` / `net_version`).
    pub const fn chain_id(&self) -> u64 {
        self.chain_id
    }

    /// The opt-in proxy upstream URL, if any (`docs/plan.md` ADR-0012).
    pub fn proxy_upstream(&self) -> Option<&str> {
        self.proxy_upstream.as_deref()
    }

    /// A plain HTTP client for the proxy path — see the field's docs.
    pub(crate) fn proxy_http(&self) -> &reqwest::Client {
        &self.proxy_http
    }

    /// Our head: the PIR server's current block (`docs/plan.md` ADR-0007 —
    /// `"latest"` means *our* head, since we follow `finalized`, not the
    /// public chain's own `"latest"`). Used both for `eth_blockNumber` and
    /// to validate `eth_getBalance`'s block parameter.
    pub async fn head(&self) -> Result<u64, RpcError> {
        Ok(self.pir.head().await?)
    }

    /// The block this deployment's rewind-client session is currently
    /// pinned at — i.e. the session's `pending_head` (see `Session`'s
    /// docs). Advances on every [`Self::get_balance`] call's routine
    /// catch-up sync, and jumps straight to a fresh bundle's own block on
    /// a `rebootstrap`.
    ///
    /// Exposed for observability and tests; nothing in this crate reads
    /// it back through this accessor to make a decision —
    /// [`Self::get_balance`] always reads the session's own field
    /// directly, under the same lock it does everything else with.
    pub async fn pinned_block(&self) -> u64 {
        self.session.lock().await.pending_head
    }

    /// Private balance lookup: `key = keccak256(addr20)` (ADR-0008), then
    /// the full rewind — build a query, answer it over HTTP, correct the
    /// response against this client's rolling public delta, and scan
    /// (`docs/plan.md` §3.3).
    ///
    /// # Automatic re-bootstrap on `Stalled`
    ///
    /// If the client's `pending_head` has fallen out of the server's
    /// retained delta window, the underlying sync reports
    /// [`RpcError::Stalled`] rather than ever guessing (`docs/plan.md`
    /// §3.6). Previously that was fatal for the rest of the process:
    /// every subsequent call re-requested the same aged-out range and
    /// failed identically, forever, with no recovery short of a full
    /// restart. This method now treats a `Stalled` from the *first*
    /// attempt as a signal to `rebootstrap` — exactly what a
    /// freshly started process does — and tries **exactly once** more.
    /// A `Stalled` from that second attempt is returned as-is: this is a
    /// bounded retry, never a loop. An unbounded retry against a server
    /// that is advancing faster than a fresh bootstrap can complete would
    /// spin forever, re-downloading the full setup bundle (hundreds of MB
    /// to low GB at deployment scale) on every iteration.
    ///
    /// The retry restarts the whole attempt from scratch — a fresh
    /// `build_query` against the *new*, post-rebootstrap client — rather
    /// than reusing anything from the failed attempt: the old client
    /// (and any query it had in flight) is discarded wholesale when
    /// `rebootstrap` replaces the session, so there is nothing
    /// live left to reuse, and [`RisePirClient::build_query`]'s
    /// single-in-flight-query-per-segment contract means a stale
    /// in-flight query could never safely be replayed against a new
    /// client's state even if it were kept around.
    ///
    /// # Staying (roughly) caught up, and why it is only hygiene
    ///
    /// Before querying, this catches the session's `pending_head` up to
    /// `self.pir.head()` if it has fallen behind. This step is **not**
    /// required for correctness — ADR-0003's whole point is that a client
    /// pinned arbitrarily far in the past still answers correctly via the
    /// rewind — but skipping it forever would let the pending delta
    /// accumulator (and therefore every future [`RisePirClient::finish`]
    /// call's cost) grow without bound, and would eventually walk
    /// `pending_head` outside the server's retention window for no
    /// reason. So it is done here as routine upkeep, on every call,
    /// rather than on a separate schedule this crate does not otherwise
    /// need.
    ///
    /// # Errors
    ///
    /// [`RpcError::Pir`] / [`RpcError::Client`] from the underlying calls.
    /// [`RpcError::Stalled`] if the retry (see above) also hits a range
    /// that has aged out of the server's retention window.
    /// [`RpcError::DecodeFailed`] on [`Lookup::DecodeFailed`] — never
    /// returned as a balance.
    ///
    /// # Return value
    ///
    /// [`Lookup::Found(b)`](Lookup::Found) → `Ok(b)`.
    /// [`Lookup::NotFound`] → `Ok(0)` — correct only because this
    /// deployment stores a *complete* nonzero-balance set (`docs/plan.md`
    /// ADR-0015: absence ⟺ zero, exactly); otherwise
    /// [`RpcError::NotInTrackedSet`].
    pub async fn get_balance(&self, addr20: [u8; 20]) -> Result<u128, RpcError> {
        self.get_balance_inner(addr20, None).await
    }

    /// [`Self::get_balance`], recording this call's client-side timers
    /// into `sink` (see [`BalanceTimings`]).
    ///
    /// Exactly the same code path — same lock, same catch-up sync, same
    /// rewind, same re-bootstrap-and-retry, same errors. The timers are
    /// read from a monotonic [`Instant`] around steps the product
    /// already performs; nothing is added to, removed from, or reordered
    /// within the balance path to make it measurable. That is the point:
    /// a measurement of a *different* path would measure nothing worth
    /// reporting.
    ///
    /// # Errors
    ///
    /// Exactly [`Self::get_balance`]'s.
    pub async fn get_balance_timed(
        &self,
        addr20: [u8; 20],
        sink: &mut BalanceTimings,
    ) -> Result<u128, RpcError> {
        self.get_balance_inner(addr20, Some(sink)).await
    }

    /// The shared body of [`Self::get_balance`] and
    /// [`Self::get_balance_timed`] — one implementation of the
    /// lock/attempt/re-bootstrap logic, so the instrumented and
    /// uninstrumented callers can never drift apart.
    async fn get_balance_inner(
        &self,
        addr20: [u8; 20],
        mut sink: Option<&mut BalanceTimings>,
    ) -> Result<u128, RpcError> {
        let key = keccak256(&addr20);
        let mut session = self.session.lock().await;

        if let Some(s) = sink.as_deref_mut() {
            *s = BalanceTimings {
                attempts: 1,
                ..BalanceTimings::default()
            };
        }
        let first = self
            .try_get_balance(&key, &mut session, sink.as_deref_mut())
            .await;
        if let Err(RpcError::Stalled) = first {
            if !self.take_rebootstrap_slot() {
                // Within [`REBOOTSTRAP_COOLDOWN`] of the previous attempt:
                // report the stall honestly instead of paying another full
                // ~831 MB `/setup` download that the last attempt just
                // proved insufficient (ADR-0029, amended).
                return first;
            }
            self.rebootstrap(&mut session).await?;
            if let Some(s) = sink.as_deref_mut() {
                s.attempts = 2;
            }
            return self.try_get_balance(&key, &mut session, sink).await;
        }
        first
    }

    /// The PIR transport this deployment's front end talks to — exposed
    /// so an in-crate caller can reach the endpoints that carry no
    /// address at all (`GET /head`, `GET /recent`) through the *same*
    /// connection pool the balance path uses.
    pub(crate) fn pir(&self) -> &PirHttpClient {
        &self.pir
    }

    /// One follow step: read `GET /head`, and if the server has advanced
    /// past this session's accumulator, pull and ingest the coalesced
    /// delta for the gap. `Ok(None)` when already caught up.
    ///
    /// This is exactly the catch-up [`Self::get_balance`] performs
    /// inline, hoisted out so a caller can run it on its own cadence
    /// (`docs/plan.md` §3.4) rather than only as a side effect of a
    /// query — the operating point a long-lived session actually sits
    /// at. It never garbage-collects: like every other caller in this
    /// crate, it leaves the hint pinned and lets `ΔD` grow, which is the
    /// product's own behaviour (see [`Self::get_balance`]'s docs).
    ///
    /// # Errors
    ///
    /// [`RpcError::Pir`] if `/head` or `/sync` fails at the transport.
    /// [`RpcError::Stalled`] if the range has aged out of the server's
    /// retention window — never treated as "nothing to do".
    /// [`RpcError::Client`] if the ingest itself is rejected.
    pub(crate) async fn follow_once(&self) -> Result<Option<SyncFetch>, RpcError> {
        let mut session = self.session.lock().await;
        let head = self.pir.head().await?;
        if head <= session.pending_head {
            return Ok(None);
        }
        let mut fetches = Vec::new();
        self.sync_to(&mut session, head, Some(&mut fetches)).await?;
        // `sync_to` performs at most one fetch, and the guard above means
        // it performed exactly one unless it errored (which returns above).
        Ok(fetches.pop())
    }

    /// Claims the one re-bootstrap slot per [`REBOOTSTRAP_COOLDOWN`]
    /// window. Consumes the slot *before* the attempt runs, deliberately:
    /// an attempt that fails, or succeeds and immediately stalls again,
    /// still paid the full download — the cooldown meters the cost, not
    /// the success. Callers always hold the `session` tokio mutex here,
    /// so the inner `std` lock is uncontended and never held across an
    /// `.await`; a poisoned lock is recovered rather than propagated
    /// (the guarded value is one timestamp — there is no invariant a
    /// panic mid-update could tear).
    fn take_rebootstrap_slot(&self) -> bool {
        let mut guard = self
            .last_rebootstrap
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let now = std::time::Instant::now();
        match *guard {
            Some(prev) if now.duration_since(prev) < REBOOTSTRAP_COOLDOWN => false,
            _ => {
                *guard = Some(now);
                true
            }
        }
    }

    /// One attempt at [`Self::get_balance`]'s full rewind, against
    /// whatever `session` currently holds. Factored out so
    /// [`Self::get_balance`] can run it, and — on exactly one occasion —
    /// run it again from scratch after a [`Self::rebootstrap`]; see that
    /// method's docs for why a retry must restart here rather than reuse
    /// any part of a failed attempt.
    async fn try_get_balance(
        &self,
        key: &AddressHash,
        session: &mut Session,
        mut sink: Option<&mut BalanceTimings>,
    ) -> Result<u128, RpcError> {
        let server_head = self.pir.head().await?;
        self.sync_to(
            session,
            server_head,
            sink.as_deref_mut().map(|s| &mut s.sync),
        )
        .await?;

        let t_build = Instant::now();
        let (queries, ctx) = session.client.build_query(key);
        if let Some(s) = sink.as_deref_mut() {
            s.build = t_build.elapsed();
        }
        let (responses, at_block) = match self
            .pir
            .answer(
                &queries,
                &session.epoch,
                &session.reshape_row_width_per_seg,
                session.arity,
            )
            .await
        {
            Ok(ok) => ok,
            // A 409 from `/answer` is the server's lineage gate
            // (ADR-0033): this session's epoch is no longer the one being
            // served — the same "only a fresh /setup is sound" condition
            // a stalled `/sync` reports, reachable here when the server
            // re-bootstraps *between* this call's sync and its answer. Map
            // it to the same variant so `get_balance`'s single
            // rebootstrap-and-retry covers it; any other status stays a
            // plain transport error.
            Err(risepir_http::ClientError::Status { status: 409, .. }) => {
                return Err(RpcError::Stalled)
            }
            Err(e) => return Err(e.into()),
        };

        // See `Self::get_balance`'s docs' "second, load-bearing sync"
        // reasoning (`docs/plan.md` ADR-0006): bring the session's
        // pending delta up to exactly the block this response was
        // answered at, in case the server advanced again since
        // `server_head` was read above. Because the whole method holds
        // the session lock throughout, nothing else can move
        // `pending_head` in between, so this second sync is always
        // sufficient — no retry loop needed for *this* race, only for a
        // genuine `Stalled` (handled one level up, in `get_balance`).
        self.sync_to(session, at_block, sink.as_deref_mut().map(|s| &mut s.sync))
            .await?;

        if let Some(s) = sink.as_deref_mut() {
            s.at_block = at_block;
            s.pinned_block = session.client.pinned_block();
            s.delta_cells = session.client.delta_cells();
        }
        // One `finish`, two shapes: instrumented callers get ADR-0003's
        // four steps timed individually, everyone else runs the
        // uninstrumented path with no clock read at all. Same method
        // underneath (`finish` *is* `finish_observed` with a no-op
        // observer), so the two can never diverge.
        let t_finish = Instant::now();
        let mut parts = FinishTimings::new();
        let outcome = match sink.as_deref_mut() {
            Some(_) => session
                .client
                .finish_observed(key, &ctx, responses, at_block, &mut parts),
            None => session.client.finish(key, &ctx, responses, at_block),
        };
        let finish_elapsed = t_finish.elapsed();
        if let Some(s) = sink.as_deref_mut() {
            s.finish = finish_elapsed;
            s.finish_parts = parts;
        }

        let lookup = outcome.map_err(|e| match e {
            // `finish` cannot actually observe a mismatch here (the sync
            // immediately above guarantees `pending_head == at_block`
            // before this call), but the mapping is kept rather than
            // `unreachable!()`-ing: `docs/plan.md`'s invariant is "never
            // guess, never panic on live input" even for a branch this
            // method's own logic should have already foreclosed.
            risepir_client::ClientError::ResponseBlockMismatch { .. } => RpcError::Stalled,
            other => RpcError::from(other),
        })?;

        // Last use of `sink` in this method, so it is moved rather than
        // reborrowed.
        if let Some(s) = sink {
            s.found = matches!(lookup, Lookup::Found(_));
        }

        match lookup {
            Lookup::Found(balance) => Ok(balance),
            Lookup::NotFound if session.strict_not_found => Err(RpcError::NotInTrackedSet),
            Lookup::NotFound => Ok(0),
            Lookup::DecodeFailed => Err(RpcError::DecodeFailed),
        }
    }

    /// Re-bootstrap `session` from scratch: re-fetch `GET /mode` **and**
    /// `GET /setup`, then replace *every* field of `*session` with what
    /// those fresh responses say (via [`Session::from_bundle`]) — a new
    /// client pinned at exactly the new bundle's own block, this
    /// deployment's current geometry, and `strict_not_found` re-derived
    /// from the freshly-fetched mode. This is exactly what a freshly
    /// started process does (see [`Self::from_setup`]), so it introduces
    /// no new trust or correctness surface, provided — as here — it
    /// replaces the whole session atomically under the session lock
    /// rather than field by field, and re-derives everything from the
    /// deployment rather than reusing anything from the session being
    /// replaced.
    ///
    /// # Why `/mode` must be re-fetched, not just `/setup`
    ///
    /// This is a binding-rule matter, not politeness. Suppose a
    /// deployment were restarted from a complete snapshot down to
    /// `--partial` (or vice versa) at some point before this
    /// re-bootstrap. A session that kept its *old* `strict_not_found`
    /// while only refreshing the hint from a new `/setup` would misapply
    /// the *new* deployment's `NotFound` policy against the *new* data —
    /// concretely, a stale `strict_not_found = false` would answer `0x0`
    /// for an account the now-partial deployment simply has not tracked
    /// yet, which is a silently wrong balance: exactly what `docs/plan.md`'s
    /// "never return a wrong answer" invariant forbids (ADR-0015/0017).
    /// So `complete` is always re-derived here, never carried over from
    /// the session being replaced — and since ADR-0033 it is read from
    /// the *same* `/setup` response as the hint (`x-risepir-mode`), so
    /// the pair cannot even straddle a server restart; a separate
    /// `GET /mode` is only ever a fallback against servers predating the
    /// header.
    ///
    /// # Errors
    ///
    /// [`RpcError::Pir`] if either fetch fails. Never itself returns
    /// [`RpcError::Stalled`] — there is no "old" state for that variant
    /// to be relative to here; a failure here is a plain transport
    /// error.
    async fn rebootstrap(&self, session: &mut Session) -> Result<(), RpcError> {
        let old_pinned = session.pending_head;
        // Mode and bundle from ONE response (`x-risepir-mode`, ADR-0033)
        // whenever the server provides it — a separate `GET /mode` can
        // race a server restart and pair one deployment's completeness
        // policy with another's data. The fallback second request exists
        // only for servers predating the header, which cannot be
        // distinguished from-the-wire from "no header"; the race window
        // it reopens is the pre-ADR-0033 status quo, not a regression.
        let (bundle, header_mode) = self.pir.setup_with_mode().await?;
        let complete = match header_mode {
            Some(m) => m,
            None => self.pir.mode().await?,
        };
        let new_session = Session::from_bundle(bundle, self.value_codec, complete);
        let new_pinned = new_session.pending_head;
        *session = new_session;
        logln!(
            "risepir-rpc: re-bootstrapped after falling out of the server's retained delta window \
             (pinned block {old_pinned} -> {new_pinned}, mode {})",
            if complete { "complete" } else { "partial" }
        );
        Ok(())
    }

    /// Bring `session`'s pending delta accumulator up to exactly
    /// `target`, pulling the coalesced `(pending_head, target]` delta over
    /// HTTP if needed. A no-op if already caught up (`target <=
    /// pending_head`).
    ///
    /// # Errors
    ///
    /// [`RpcError::Pir`] if the HTTP call itself fails.
    /// [`RpcError::Stalled`] if the server reports the range as outside
    /// its retention window (`PirHttpClient::sync` returning `Ok(None)`)
    /// — never treated as "nothing to do".
    /// [`RpcError::Client`] if [`RisePirClient::ingest_delta`] itself
    /// rejects the delta (should not happen given the strict
    /// `pending_head`-gated call pattern above, but not assumed away).
    async fn sync_to(
        &self,
        session: &mut Session,
        target: u64,
        sink: Option<&mut Vec<SyncFetch>>,
    ) -> Result<(), RpcError> {
        if target <= session.pending_head {
            return Ok(());
        }
        let from = session.pending_head;
        // Snapshot either side of the one `/sync` call to attribute its
        // wire/decode/bytes to *this* fetch rather than to the trial's
        // running total (see `PirHttpClient::net_stats`). `None` when
        // uninstrumented, and then nothing below reads a clock.
        let before = sink.as_ref().and_then(|_| self.pir.net_stats());
        match self
            .pir
            .sync(
                from,
                target,
                &session.epoch,
                session.plaintext_bits,
                session.arity as u32,
            )
            .await?
        {
            Some(delta) => {
                let new_head = delta.block;
                // Counted before the ingest consumes the delta, so it can
                // only be done here — inside the caller's own timed
                // region. It is O(cells fetched) and lands in the
                // caller's residual on a syncing trial; see
                // `BalanceTimings`' docs.
                let cells_in_delta = sink.as_ref().map_or(0, |_| delta_cells(&delta));
                let t_ingest = Instant::now();
                session.client.ingest_delta(&delta)?;
                let ingest = t_ingest.elapsed();
                session.pending_head = new_head;
                if let Some(s) = sink {
                    let after = self.pir.net_stats();
                    let (wire, decode, wire_bytes) = match (before, after) {
                        (Some(b), Some(a)) => (
                            Duration::from_nanos(a.sync.wire_ns.saturating_sub(b.sync.wire_ns)),
                            Duration::from_nanos(a.sync.decode_ns.saturating_sub(b.sync.decode_ns)),
                            a.sync.response_bytes.saturating_sub(b.sync.response_bytes),
                        ),
                        _ => (Duration::ZERO, Duration::ZERO, 0),
                    };
                    s.push(SyncFetch {
                        from_block: from,
                        to_block: new_head,
                        blocks: new_head.saturating_sub(from),
                        received_at_unix_ms: std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map_or(0, |d| d.as_millis()),
                        wire,
                        decode,
                        wire_bytes,
                        ingest,
                        cells_in_delta,
                        delta_cells_total: session.client.delta_cells(),
                    });
                }
                Ok(())
            }
            None => Err(RpcError::Stalled),
        }
    }
}

/// Nonzero `(segment, row, offset)` cells carried by one delta.
///
/// A pure count over the delta's own sparse structure — never the values.
fn delta_cells(delta: &risepir_proto::BlockDelta) -> usize {
    delta
        .per_segment
        .iter()
        .flat_map(|rows| rows.iter())
        .map(|(_, cells)| cells.len())
        .sum()
}
