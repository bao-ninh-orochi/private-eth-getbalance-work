//! [`PrivateEth`] — the PIR-backed private-`eth_getBalance` core: one
//! `Mutex`-guarded [`RisePirClient`] talking to one [`PirHttpClient`]
//! (`docs/plan.md` §3.3/§3.6, ADR-0003, ADR-0006, ADR-0010).

use std::sync::atomic::{AtomicU64, Ordering};

use ikpir_common::SimplePirBackend;
use risepir_client::{Lookup, RisePirClient};
use risepir_http::PirHttpClient;

use crate::error::RpcError;
use risepir_proto::keccak256;

/// The private `eth_getBalance` core for one RisePIR deployment.
///
/// # Why `pending_head` exists alongside the client, not inside it
///
/// [`RisePirClient::pinned_block`] is the block the client's *hint* is
/// pinned at — it only moves on an explicit `collect_garbage` (never
/// called by this crate; see [`Self::get_balance`]'s docs). What this type
/// actually needs to track is the block its **pending delta accumulator**
/// currently covers up to: [`RisePirClient::ingest_delta`] requires each
/// new delta to extend that accumulator strictly forward, and
/// [`RisePirClient::finish`] requires the response's `at_block` to equal
/// it *exactly* (`docs/plan.md` §3.3 step 2 — "the response names the
/// epoch," ADR-0006). `RisePirClient` does not expose that head as a
/// public accessor (deliberately — it is accumulator-internal
/// bookkeeping, not part of the rewind's public contract), so this type
/// tracks its own copy, updated in lockstep with the client under the same
/// lock (see [`Self::sync_to`]).
///
/// # Concurrency (ADR-0010)
///
/// One shared `Mutex<RisePirClient>`; [`Self::get_balance`] holds the lock
/// for its entire body, including the network round trips to
/// `self.pir` — never just around the in-memory client calls. This is
/// required, not merely convenient: [`RisePirClient::build_query`]
/// documents a single-in-flight-query-per-segment contract (a second
/// `build_query` before the first's matching `finish` discards the first
/// query's secret), so two `get_balance` calls interleaving their
/// `build_query`/`answer`/`finish` steps would corrupt each other's
/// lookups. `pending_head` is read/written only while this same lock is
/// held (see its docs above) — no second lock, no lock-ordering hazard.
pub struct PrivateEth {
    pub(crate) client: tokio::sync::Mutex<RisePirClient<SimplePirBackend>>,
    /// See the struct docs' "why `pending_head` exists" section. Invariant:
    /// only ever read or written while `client`'s lock is held.
    pub(crate) pending_head: AtomicU64,
    pub(crate) pir: PirHttpClient,
    /// This deployment's per-segment `reshape_row_width` — geometry
    /// [`PirHttpClient::answer`] needs to decode a response bundle but
    /// does not itself store (`risepir-http`'s `PirHttpClient` docs).
    pub(crate) reshape_row_width_per_seg: Vec<u32>,
    pub(crate) arity: usize,
    /// This deployment's SCF `plaintext_bits` — geometry
    /// [`PirHttpClient::sync`] needs to decode a coalesced delta but does
    /// not itself store, for the same reason as `reshape_row_width_per_seg`
    /// above.
    pub(crate) plaintext_bits: u32,
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

    /// Private balance lookup: `key = keccak256(addr20)` (ADR-0008), then
    /// the full rewind — build a query, answer it over HTTP, correct the
    /// response against this client's rolling public delta, and scan
    /// (`docs/plan.md` §3.3).
    ///
    /// # Staying (roughly) caught up, and why it is only hygiene
    ///
    /// Before querying, this catches the client's `pending_head` up to
    /// `self.pir.head()` if it has fallen behind. This step is **not**
    /// required for correctness — ADR-0003's whole point is that a client
    /// pinned arbitrarily far in the past still answers correctly via the
    /// rewind — but skipping it forever would let the pending delta
    /// accumulator (and therefore every future [`RisePirClient::finish`]
    /// call's cost) grow without bound, and would eventually walk the
    /// client's `pending_head` outside the server's retention window for
    /// no reason. So it is done here as routine upkeep, on every call,
    /// rather than on a separate schedule this crate does not otherwise
    /// need.
    ///
    /// # The second, load-bearing sync: racing the block-follow loop
    ///
    /// [`risepir_server::RisePirServer::answer`] always answers "at the
    /// server's current head" — which, because a background follower is
    /// concurrently applying new blocks, can have advanced *again* between
    /// the `self.pir.head()` read above and the moment `self.pir.answer`
    /// actually executes. [`RisePirClient::finish`] requires its `at_block`
    /// argument to equal the client's `pending_head` *exactly*
    /// ("the response names the epoch," ADR-0006) — so after `answer`
    /// returns, this catches `pending_head` up to exactly that call's own
    /// `at_block` (a second, surgical [`Self::sync_to`], never a guess)
    /// before calling `finish`. Because the whole method holds `client`'s
    /// lock throughout, nothing else can move `pending_head` in between,
    /// so this second sync is always sufficient — no retry loop needed.
    ///
    /// # Errors
    ///
    /// [`RpcError::Pir`] / [`RpcError::Client`] from the underlying calls.
    /// [`RpcError::Stalled`] if a needed sync's range has aged out of the
    /// server's retention window (`docs/plan.md` §3.6: "never guess").
    /// [`RpcError::DecodeFailed`] on [`Lookup::DecodeFailed`] — never
    /// returned as a balance.
    ///
    /// # Return value
    ///
    /// [`Lookup::Found(b)`](Lookup::Found) → `Ok(b)`.
    /// [`Lookup::NotFound`] → `Ok(0)` — correct only because this
    /// deployment stores a *complete* nonzero-balance set (`docs/plan.md`
    /// ADR-0015: absence ⟺ zero, exactly).
    pub async fn get_balance(&self, addr20: [u8; 20]) -> Result<u128, RpcError> {
        let key = keccak256(&addr20);
        let mut client = self.client.lock().await;

        let server_head = self.pir.head().await?;
        self.sync_to(&mut client, server_head).await?;

        let (queries, ctx) = client.build_query(&key);
        let (responses, at_block) = self.pir.answer(&queries, &self.reshape_row_width_per_seg, self.arity).await?;

        // See the method docs' "second, load-bearing sync" section: bring
        // the client's pending delta up to exactly the block this
        // response was answered at, in case the server advanced again
        // since `server_head` was read above.
        self.sync_to(&mut client, at_block).await?;

        let lookup = client.finish(&key, &ctx, responses, at_block).map_err(|e| match e {
            // `finish` cannot actually observe a mismatch here (the sync
            // immediately above guarantees `pending_head == at_block`
            // before this call), but the mapping is kept rather than
            // `unreachable!()`-ing: `docs/plan.md`'s invariant is "never
            // guess, never panic on live input" even for a branch this
            // method's own logic should have already foreclosed.
            risepir_client::ClientError::ResponseBlockMismatch { .. } => RpcError::Stalled,
            other => RpcError::from(other),
        })?;

        match lookup {
            Lookup::Found(balance) => Ok(balance),
            Lookup::NotFound => Ok(0),
            Lookup::DecodeFailed => Err(RpcError::DecodeFailed),
        }
    }

    /// Bring `client`'s pending delta accumulator up to exactly `target`,
    /// pulling the coalesced `(pending_head, target]` delta over HTTP if
    /// needed. A no-op if already caught up (`target <= pending_head`).
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
    async fn sync_to(&self, client: &mut RisePirClient<SimplePirBackend>, target: u64) -> Result<(), RpcError> {
        let pending = self.pending_head.load(Ordering::SeqCst);
        if target <= pending {
            return Ok(());
        }
        match self.pir.sync(pending, target, self.plaintext_bits, self.arity as u32).await? {
            Some(delta) => {
                let new_head = delta.block;
                client.ingest_delta(&delta)?;
                self.pending_head.store(new_head, Ordering::SeqCst);
                Ok(())
            }
            None => Err(RpcError::Stalled),
        }
    }
}
