//! Bounded, strictly in-order block prefetch for the mainnet follow loop
//! (ADR-0047).
//!
//! # The problem
//!
//! The follow loop's cost is dominated by the *fetch*, not the apply: one
//! `block_update(n)` is two JSON-RPC round trips to a keyless provider
//! (~1–2 s for `debug_traceBlockByNumber` with the prestate tracer, plus
//! dRPC's deterministic `HTTP 408` refusals on heavy blocks, ADR-0024),
//! while [`risepir_http::NodeState::apply_block`] takes ~4 ms at the
//! complete set. A restart that has to replay 52,000 blocks therefore
//! spends ~13 h waiting on the network with the CPU idle. Prefetching
//! overlaps those waits; nothing else about the loop changes.
//!
//! # The invariants this type exists to hold
//!
//! 1. **Strictly in order, nothing skipped.** The applier always asks for
//!    exactly one block number, and [`BlockPrefetch::fetch`] returns *that*
//!    block or an error — never a later one that happened to finish first.
//!    A block whose fetch failed is retried by the caller with the same
//!    `n`, which re-issues the same [`BlockSource::block_update`] call;
//!    blocks already fetched behind it simply wait, still in flight, and
//!    are neither re-fetched nor consumed out of order. This is the
//!    never-wrong-answer rule (`CLAUDE.md`) applied to scheduling: a
//!    skipped block is a wrong balance.
//! 2. **At most `depth` fetches in flight.** The window is
//!    `n ..= min(n + depth - 1, head)`, so the number of concurrently
//!    outstanding requests never exceeds `depth` — a bound on the load
//!    this deployment puts on someone else's keyless endpoint, not just on
//!    memory.
//! 3. **Never past the head being followed.** `head` is the `finalized`
//!    block the loop last polled ([`BlockPrefetch::set_head`]); the
//!    lookahead is clipped to it. Prefetching is only safe *because* of
//!    that clip: `finalized` cannot reorg (ADR-0007), so a block fetched
//!    ahead of time can never turn out to have been the wrong block.
//! 4. **`depth == 1` is the pre-prefetch loop.** One fetch is issued, and
//!    awaited, per applied block, in block order, with the identical retry
//!    behaviour — see `depth_one_reproduces_the_pre_prefetch_call_sequence`,
//!    which pins that against a driver written the way the loop was before
//!    this module existed.
//!
//! # What it deliberately does not touch
//!
//! Fetch tasks do nothing but call the feed: they never take
//! `NodeState`'s lock, never journal, never save. Apply, reconcile,
//! autosave (ADR-0025) and the journal (ADR-0026) all still run on the
//! follow loop's own task, one block at a time, in the same order as
//! before — so ADR-0025's "the applier's own task is the only writer"
//! argument is untouched.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use risepir_feed::rpc::{FetchedBlock, RpcFeed};
use risepir_feed::FeedError;
use tokio::task::JoinHandle;

/// "Fetch one finalized block's update" — the single seam
/// [`BlockPrefetch`] needs from the feed, so the scheduling logic can be
/// unit-tested against an in-memory mock with scripted per-block
/// latencies and failures instead of a live provider. Implemented for
/// [`RpcFeed`] as a straight forward to its inherent
/// [`RpcFeed::block_update`], which keeps the ordered endpoint chain and
/// its failure semantics (ADR-0024) exactly as they are: a retry is the
/// same call, against the same chain, in the same order.
///
/// Declared as an explicit `-> impl Future<...> + Send` rather than a
/// sugared `async fn` for the same reason `mainnet::ConfirmSource` is: the
/// returned future is spawned, so it must be provably `Send`.
pub(crate) trait BlockSource: Send + Sync + 'static {
    /// One finalized block's [`FetchedBlock`], exactly as
    /// [`RpcFeed::block_update`] produces it.
    fn block_update(
        &self,
        n: u64,
    ) -> impl std::future::Future<Output = Result<FetchedBlock, FeedError>> + Send;
}

impl BlockSource for RpcFeed {
    fn block_update(
        &self,
        n: u64,
    ) -> impl std::future::Future<Output = Result<FetchedBlock, FeedError>> + Send {
        // Inherent method (inherent impls win over trait impls in path
        // resolution) — this is the real endpoint-chain walk, not a
        // recursive call into this trait method.
        RpcFeed::block_update(self, n)
    }
}

/// Why a prefetched block did not arrive. Both variants mean exactly what
/// a failed fetch has always meant to the follow loop — "try again, same
/// block" — and neither is ever a reason to move on.
pub(crate) enum FetchFailure {
    /// The feed declined or failed: whatever [`BlockSource::block_update`]
    /// returned. Displayed verbatim, so the loop's
    /// `block {n} fetch failed ({e}); retrying` line is byte-identical to
    /// the one it printed before this module existed.
    Feed(FeedError),
    /// The spawned fetch task did not run to completion — it panicked, or
    /// was aborted. Distinguished only so the log names the real cause;
    /// the loop's reaction is identical (retry the same block). Retrying
    /// is the safe direction: a panic inside a fetch is evidence about the
    /// fetch, never about the chain, and the alternative — giving up on
    /// the block — is exactly the skip this deployment may never take.
    Task(String),
}

impl fmt::Display for FetchFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Feed(e) => write!(f, "{e}"),
            Self::Task(detail) => write!(f, "prefetch task did not complete: {detail}"),
        }
    }
}

/// A bounded, in-order lookahead over [`BlockSource::block_update`]: at
/// most `depth` fetches in flight, never above `head`, always handed to
/// the applier in strictly increasing block order. See the module docs
/// for the invariants.
pub(crate) struct BlockPrefetch<F: BlockSource> {
    feed: Arc<F>,
    /// Window width in blocks, `>= 1`. `1` disables lookahead entirely
    /// (one fetch issued and awaited per applied block).
    depth: usize,
    /// The `finalized` block the follow loop is currently following. No
    /// *lookahead* fetch is ever issued above it.
    head: u64,
    /// In-flight (or finished-but-not-yet-collected) fetches, keyed by
    /// block number. Ordered so the head-bound sweep in [`Self::set_head`]
    /// is a single `split_off`.
    inflight: BTreeMap<u64, JoinHandle<Result<FetchedBlock, FeedError>>>,
}

impl<F: BlockSource> BlockPrefetch<F> {
    /// A prefetcher over `feed` with a window of `depth` blocks. `depth`
    /// is clamped to at least 1 — a window of zero blocks would be a loop
    /// that never fetches anything, which is worse than any value an
    /// operator could have meant by it. `head` starts at 0; the follow
    /// loop calls [`Self::set_head`] with `finalized` before asking for
    /// any block.
    pub(crate) fn new(feed: Arc<F>, depth: usize) -> Self {
        Self {
            feed,
            depth: depth.max(1),
            head: 0,
            inflight: BTreeMap::new(),
        }
    }

    /// The effective window width (post-clamp). Test-only: the follow
    /// loop logs the depth it *asked* for, at startup, from its own
    /// config.
    #[cfg(test)]
    pub(crate) fn depth(&self) -> usize {
        self.depth
    }

    /// Publishes the `finalized` head the loop is now following, widening
    /// the lookahead.
    ///
    /// `finalized` never regresses (ADR-0007), so the sweep below is
    /// belt-and-braces: were it ever to, the lookahead that is now above
    /// the head is aborted rather than left running past the head this
    /// loop follows — invariant 3 holds unconditionally, not just while
    /// the chain behaves.
    pub(crate) fn set_head(&mut self, head: u64) {
        self.head = head;
        for (_, handle) in self.inflight.split_off(&head.saturating_add(1)) {
            handle.abort();
        }
    }

    /// Block `n`'s update: the already-running fetch for it if there is
    /// one, otherwise a fresh one, awaited to completion. Tops the window
    /// up to `depth` first, so the blocks after `n` are already in flight
    /// while the caller applies `n`.
    ///
    /// `n` itself is always fetched, head or no head: it is the block the
    /// loop is applying, and the loop only ever asks for a block at or
    /// below the `finalized` it polled. `head` bounds the *lookahead*
    /// past `n`.
    ///
    /// On `Err` the block's slot is cleared, so the caller's retry
    /// (same `n`, after the usual pause) issues the same
    /// [`BlockSource::block_update`] call again. Every other in-flight
    /// fetch is untouched by that failure — and unreachable by the
    /// applier until `n` succeeds.
    pub(crate) async fn fetch(&mut self, n: u64) -> Result<FetchedBlock, FetchFailure> {
        debug_assert!(
            self.inflight.keys().all(|&b| b >= n),
            "the applier asks for strictly increasing block numbers; a lower one in flight means a block was skipped"
        );
        let through = n
            .saturating_add(self.depth as u64 - 1)
            .min(self.head)
            .max(n);
        for b in n..=through {
            self.inflight.entry(b).or_insert_with(|| {
                let feed = Arc::clone(&self.feed);
                tokio::spawn(async move { feed.block_update(b).await })
            });
        }

        // Awaited through `&mut` rather than by removing the handle
        // first: if this future is ever dropped mid-await, the handle
        // stays in the map and the fetch is collected by the next call
        // instead of being lost and re-issued.
        let handle = self
            .inflight
            .get_mut(&n)
            .expect("the window always contains n itself");
        let joined = handle.await;
        self.inflight.remove(&n);

        match joined {
            Ok(Ok(block)) => Ok(block),
            Ok(Err(e)) => Err(FetchFailure::Feed(e)),
            Err(join) => Err(FetchFailure::Task(join.to_string())),
        }
    }

    /// How many fetches are currently outstanding — the quantity
    /// invariant 2 bounds. Test-only: the follow loop never needs it.
    #[cfg(test)]
    pub(crate) fn in_flight(&self) -> usize {
        self.inflight.len()
    }
}

impl<F: BlockSource> Drop for BlockPrefetch<F> {
    /// The follow loop returns (and drops this) on a `CRITICAL` apply or
    /// reconcile failure. Aborting the lookahead there stops up to
    /// `depth - 1` requests that nothing will ever collect, rather than
    /// leaving them to finish against a provider this deployment has just
    /// stopped following.
    fn drop(&mut self) {
        for (_, handle) in std::mem::take(&mut self.inflight) {
            handle.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;
    use std::time::Duration;

    use risepir_proto::BlockUpdate;

    /// An empty-but-well-formed block: these tests are about *scheduling*,
    /// so the only field any of them reads is the block number.
    fn block(n: u64) -> FetchedBlock {
        FetchedBlock {
            update: BlockUpdate {
                block: n,
                changes: Vec::new(),
                credits: Vec::new(),
            },
            changed: Vec::new(),
            credited: Vec::new(),
        }
    }

    /// A [`BlockSource`] with scripted per-block latency and per-block
    /// failures, which records the exact sequence of calls it received and
    /// the high-water mark of concurrent calls.
    ///
    /// Every test drives it under `#[tokio::test(start_paused = true)]`, so
    /// the latencies are *logical* time that tokio auto-advances when every
    /// task is parked: the orderings below are exact, not wall-clock races.
    #[derive(Default)]
    struct MockSource {
        latency_ms: HashMap<u64, u64>,
        default_latency_ms: u64,
        /// Remaining scripted failures per block, consumed one per call.
        fails: Mutex<HashMap<u64, usize>>,
        /// Remaining scripted task panics per block, consumed one per call.
        panics: Mutex<HashMap<u64, usize>>,
        /// Every `block_update` call, in the order the calls were issued.
        calls: Mutex<Vec<u64>>,
        in_flight: AtomicUsize,
        max_in_flight: AtomicUsize,
    }

    impl MockSource {
        fn with_default_latency(ms: u64) -> Self {
            Self {
                default_latency_ms: ms,
                ..Self::default()
            }
        }

        fn latency(mut self, n: u64, ms: u64) -> Self {
            self.latency_ms.insert(n, ms);
            self
        }

        fn failing(self, n: u64, times: usize) -> Self {
            self.fails.lock().unwrap().insert(n, times);
            self
        }

        fn panicking(self, n: u64, times: usize) -> Self {
            self.panics.lock().unwrap().insert(n, times);
            self
        }

        fn calls(&self) -> Vec<u64> {
            self.calls.lock().unwrap().clone()
        }

        fn max_in_flight(&self) -> usize {
            self.max_in_flight.load(Ordering::SeqCst)
        }

        /// How many times block `n` was fetched.
        fn call_count(&self, n: u64) -> usize {
            self.calls().iter().filter(|&&b| b == n).count()
        }

        /// Decrements a scripted counter, reporting whether this call is
        /// one of the scripted ones.
        fn take(counter: &Mutex<HashMap<u64, usize>>, n: u64) -> bool {
            let mut c = counter.lock().unwrap();
            match c.get_mut(&n) {
                Some(left) if *left > 0 => {
                    *left -= 1;
                    true
                }
                _ => false,
            }
        }
    }

    impl BlockSource for MockSource {
        // Sugared `async fn`, like `mainnet::ConfirmSource`'s own test
        // stubs: the compiler still checks the desugared future against
        // the trait's `+ Send` bound.
        async fn block_update(&self, n: u64) -> Result<FetchedBlock, FeedError> {
            // Locks are taken and released before the await: holding one
            // across it would make this future non-`Send`.
            self.calls.lock().unwrap().push(n);
            let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_in_flight.fetch_max(now, Ordering::SeqCst);
            let panics = Self::take(&self.panics, n);
            let fails = Self::take(&self.fails, n);
            let ms = self
                .latency_ms
                .get(&n)
                .copied()
                .unwrap_or(self.default_latency_ms);

            tokio::time::sleep(Duration::from_millis(ms)).await;

            self.in_flight.fetch_sub(1, Ordering::SeqCst);
            if panics {
                panic!("scripted panic fetching block {n}");
            }
            if fails {
                return Err(FeedError::Internal(format!("scripted failure block {n}")));
            }
            Ok(block(n))
        }
    }

    /// The applier, written exactly the way `mainnet::follow_loop` drives
    /// the prefetcher: ask for `last + 1`, retry the same block on any
    /// failure, never move on until it applies. Returns the blocks in
    /// apply order.
    async fn drive(pf: &mut BlockPrefetch<MockSource>, start: u64, finalized: u64) -> Vec<u64> {
        let mut applied = Vec::new();
        let mut last = start;
        pf.set_head(finalized);
        while last < finalized {
            let n = last + 1;
            match pf.fetch(n).await {
                Ok(fetched) => {
                    applied.push(fetched.update.block);
                    last = n;
                }
                Err(_) => continue, // same n, idempotent — exactly the loop's own retry
            }
        }
        applied
    }

    /// The *pre-prefetch* applier: `feed.block_update(n)` awaited inline,
    /// retried on failure. This is what `follow_loop`'s inner body was
    /// before ADR-0047, kept here as the reference `--prefetch 1` is
    /// pinned against.
    async fn drive_without_prefetch(feed: &MockSource, start: u64, finalized: u64) -> Vec<u64> {
        let mut applied = Vec::new();
        let mut last = start;
        while last < finalized {
            let n = last + 1;
            match feed.block_update(n).await {
                Ok(fetched) => {
                    applied.push(fetched.update.block);
                    last = n;
                }
                Err(_) => continue,
            }
        }
        applied
    }

    /// Invariants 1 and 2 together, which is the point of the whole
    /// module: with a window of 4 and wildly uneven per-block latencies
    /// (block 3 is 20× slower than its neighbours, so blocks 4–6 finish
    /// long before it), blocks are still applied in strictly increasing
    /// order, every block exactly once, and the number of concurrent
    /// fetches never exceeds the window.
    #[tokio::test(start_paused = true)]
    async fn applies_in_order_with_at_most_depth_fetches_in_flight() {
        let feed = Arc::new(
            MockSource::with_default_latency(5)
                .latency(3, 100)
                .latency(7, 60),
        );
        let mut pf = BlockPrefetch::new(Arc::clone(&feed), 4);

        let applied = drive(&mut pf, 0, 12).await;

        assert_eq!(applied, (1..=12).collect::<Vec<_>>());
        assert!(
            feed.max_in_flight() <= 4,
            "window of 4 must never have more than 4 fetches outstanding, saw {}",
            feed.max_in_flight()
        );
        assert!(
            feed.max_in_flight() >= 2,
            "no concurrency actually happened, so this test proves nothing about ordering under it"
        );
        for n in 1..=12 {
            assert_eq!(
                feed.call_count(n),
                1,
                "block {n} was fetched more than once"
            );
        }
    }

    /// A failing block holds every later block, however ready they are
    /// (invariant 1). Block 2 fails twice and its neighbours are fast, so
    /// blocks 3–5 are sitting finished in the window across both retries.
    /// Nothing is applied out of order, nothing is skipped, nothing is
    /// applied twice — and the two retries re-fetch **only** block 2,
    /// leaving the ready blocks alone.
    #[tokio::test(start_paused = true)]
    async fn a_failing_block_holds_later_ready_blocks_and_skips_nothing() {
        let feed = Arc::new(
            MockSource::with_default_latency(5)
                .latency(2, 50)
                .failing(2, 2),
        );
        let mut pf = BlockPrefetch::new(Arc::clone(&feed), 4);

        let applied = drive(&mut pf, 0, 6).await;

        assert_eq!(applied, vec![1, 2, 3, 4, 5, 6]);
        assert_eq!(feed.call_count(2), 3, "two failures then a success");
        for n in [1, 3, 4, 5, 6] {
            assert_eq!(
                feed.call_count(n),
                1,
                "block {n} was disturbed by block 2's retries"
            );
        }
        // Blocks 3 and 4 were fetched *while* block 2 was still failing —
        // i.e. they really were ready and really did wait.
        let calls = feed.calls();
        let first_3 = calls.iter().position(|&b| b == 3).expect("block 3 fetched");
        let last_2 = calls
            .iter()
            .rposition(|&b| b == 2)
            .expect("block 2 fetched");
        assert!(
            first_3 < last_2,
            "block 3 must have been in flight before block 2's final attempt: {calls:?}"
        );
    }

    /// Invariant 3: the lookahead is clipped to the head the loop is
    /// following, however large the window. With `depth = 32` and a head
    /// of 3, blocks 4+ must not be touched — the loop has not been told
    /// they are finalized yet. When the head advances, the window opens
    /// up, and still no further.
    #[tokio::test(start_paused = true)]
    async fn never_fetches_above_the_head_being_followed() {
        let feed = Arc::new(MockSource::with_default_latency(5));
        let mut pf = BlockPrefetch::new(Arc::clone(&feed), 32);

        let applied = drive(&mut pf, 0, 3).await;
        assert_eq!(applied, vec![1, 2, 3]);
        let after_first_head = feed.calls();
        assert!(
            after_first_head.iter().all(|&b| b <= 3),
            "fetched past the finalized head: {after_first_head:?}"
        );

        let applied = drive(&mut pf, 3, 6).await;
        assert_eq!(applied, vec![4, 5, 6]);
        assert!(
            feed.calls().iter().all(|&b| b <= 6),
            "fetched past the advanced head: {:?}",
            feed.calls()
        );
    }

    /// Invariant 4, pinned against the pre-ADR-0047 loop body itself: with
    /// `--prefetch 1` the sequence of `block_update` calls — including the
    /// retry of a failing block — is byte-for-byte the sequence the loop
    /// issued before this module existed, and exactly one fetch is ever
    /// outstanding.
    #[tokio::test(start_paused = true)]
    async fn depth_one_reproduces_the_pre_prefetch_call_sequence() {
        let reference = MockSource::with_default_latency(5).failing(3, 2);
        let reference_applied = drive_without_prefetch(&reference, 0, 6).await;

        let feed = Arc::new(MockSource::with_default_latency(5).failing(3, 2));
        let mut pf = BlockPrefetch::new(Arc::clone(&feed), 1);
        let applied = drive(&mut pf, 0, 6).await;

        assert_eq!(applied, reference_applied);
        assert_eq!(
            feed.calls(),
            reference.calls(),
            "--prefetch 1 must issue exactly the pre-prefetch call sequence"
        );
        assert_eq!(feed.calls(), vec![1, 2, 3, 3, 3, 4, 5, 6]);
        assert_eq!(
            feed.max_in_flight(),
            1,
            "--prefetch 1 must never have two fetches outstanding"
        );
    }

    /// A fetch task that panics is a failed fetch, not a skipped block:
    /// the loop retries the same `n` and applies it when it succeeds.
    /// (tokio prints the panic itself — that output is expected here.)
    #[tokio::test(start_paused = true)]
    async fn a_panicking_fetch_is_retried_not_skipped() {
        let feed = Arc::new(MockSource::with_default_latency(5).panicking(2, 1));
        let mut pf = BlockPrefetch::new(Arc::clone(&feed), 4);

        let applied = drive(&mut pf, 0, 4).await;

        assert_eq!(applied, vec![1, 2, 3, 4]);
        assert_eq!(feed.call_count(2), 2, "the panicking fetch was re-issued");
    }

    /// `depth` is clamped, never zero: a window of zero blocks would be a
    /// follow loop that fetches nothing at all.
    #[tokio::test(start_paused = true)]
    async fn depth_zero_is_clamped_to_one_and_still_makes_progress() {
        let feed = Arc::new(MockSource::with_default_latency(5));
        let mut pf = BlockPrefetch::new(Arc::clone(&feed), 0);
        assert_eq!(pf.depth(), 1);
        assert_eq!(drive(&mut pf, 0, 3).await, vec![1, 2, 3]);
        assert_eq!(feed.max_in_flight(), 1);
    }

    /// The window is refilled as it drains, not just at the start: after
    /// applying a block there are still `depth` fetches outstanding while
    /// blocks remain below the head, and the outstanding set is exactly
    /// the blocks after the one just applied.
    #[tokio::test(start_paused = true)]
    async fn the_window_slides_and_stays_full_while_blocks_remain() {
        let feed = Arc::new(MockSource::with_default_latency(5));
        let mut pf = BlockPrefetch::new(Arc::clone(&feed), 3);
        pf.set_head(10);

        assert_eq!(pf.fetch(1).await.map(|b| b.update.block).ok(), Some(1));
        assert_eq!(pf.in_flight(), 2, "blocks 2 and 3 stay in flight");
        assert_eq!(feed.calls(), vec![1, 2, 3]);

        assert_eq!(pf.fetch(2).await.map(|b| b.update.block).ok(), Some(2));
        assert_eq!(pf.in_flight(), 2, "blocks 3 and 4 stay in flight");
        // Block 4's fetch is *spawned* by the window refill; it reaches
        // the feed on the next yield (block 2's own fetch had already
        // finished, so awaiting it never parked this task).
        tokio::task::yield_now().await;
        assert_eq!(feed.calls(), vec![1, 2, 3, 4]);
    }

    /// Dropping the prefetcher — what the follow loop does when it stops
    /// on a `CRITICAL` — cancels the lookahead instead of leaving it
    /// running against the provider.
    #[tokio::test(start_paused = true)]
    async fn dropping_the_prefetcher_aborts_the_lookahead() {
        // Block 1 finishes long before its neighbours, so 2-4 are still
        // outstanding at the moment the prefetcher is dropped.
        let feed = Arc::new(MockSource::with_default_latency(500).latency(1, 5));
        let mut pf = BlockPrefetch::new(Arc::clone(&feed), 4);
        pf.set_head(10);
        assert!(pf.fetch(1).await.is_ok());
        assert_eq!(pf.in_flight(), 3);
        drop(pf);

        // Long enough for every aborted fetch to have completed had it
        // not been aborted (logical time — this does not sleep for real).
        tokio::time::sleep(Duration::from_millis(5_000)).await;
        assert_eq!(
            feed.in_flight.load(Ordering::SeqCst),
            3,
            "aborted tasks never reached their own completion path"
        );
    }
}
