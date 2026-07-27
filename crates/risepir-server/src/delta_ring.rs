//! [`DeltaRing`] — the server's sliding retention window over recent
//! [`BlockDelta`]s (`docs/plan.md` §3, "`DeltaRing` — the retention
//! window").

use std::collections::VecDeque;

use risepir_proto::BlockDelta;

/// Default retention window: 300 blocks — `docs/plan.md`'s "~1 hour,
/// ~15 MB" figure at Ethereum block times.
pub const DEFAULT_CAPACITY: usize = 300;

/// Sliding window of the most recent `capacity` per-block deltas.
///
/// # Purpose
///
/// Lets a client that last synced at `from_block` fetch one coalesced
/// delta up to the server's head (`sync(from_block)` in `docs/plan.md`
/// §3.4) instead of replaying every intermediate [`BlockDelta`]
/// individually.
///
/// # Rationale
///
/// This is deliberately **not** a cache of the whole delta history: a
/// client whose `from_block` has aged out of the window cannot be served
/// a correct coalesced delta (part of the history it would need is
/// gone), and the only sound response is [`None`], forcing a caller-side
/// full resync — never a delta that silently omits the missing part.
/// This mirrors the crate's "never return a wrong answer" invariant
/// (`docs/plan.md`, top of file) applied to the retention window
/// specifically.
pub struct DeltaRing {
    capacity: usize,
    buf: VecDeque<BlockDelta>,
}

impl DeltaRing {
    /// New, empty ring retaining at most `capacity` blocks.
    ///
    /// # Panics
    ///
    /// If `capacity == 0` — a zero-capacity ring could never serve a
    /// single [`Self::range`] query, which is certainly a caller bug
    /// rather than a runtime condition to recover from.
    pub fn new(capacity: usize) -> Self {
        assert!(capacity > 0, "DeltaRing capacity must be >= 1");
        Self {
            capacity,
            buf: VecDeque::with_capacity(capacity),
        }
    }

    /// Push the next block's delta, evicting the oldest if already at
    /// capacity.
    ///
    /// # Constraints
    ///
    /// Callers should push in the same strictly increasing block order
    /// that [`crate::RisePirServer::apply_block`] enforces (ideally
    /// gap-free, i.e. one push per successful `apply_block` call).
    /// `DeltaRing` has no reference to "the" server and does not verify
    /// this itself, but [`Self::range`] cannot produce a contiguous
    /// coalesced result across a gap it was never told about — it
    /// returns [`None`] rather than silently skipping the missing block.
    pub fn push(&mut self, d: BlockDelta) {
        if self.buf.len() == self.capacity {
            self.buf.pop_front();
        }
        self.buf.push_back(d);
    }

    /// Coalesced delta for `(from_block, to_block]`.
    ///
    /// # Returns
    ///
    /// `Some(delta)` with `delta.block == to_block` if every block in
    /// `from_block+1 ..= to_block` is currently retained, coalesced via
    /// [`BlockDelta::coalesce`].
    ///
    /// `None` if any of the following holds — all three are folded into
    /// the same `None` on purpose, so this method never returns a
    /// coalesced delta that silently omits part of the requested range:
    /// - `from_block >= to_block` (nothing to coalesce, or a backwards
    ///   range);
    /// - `from_block` (or any block after it) has aged out of the window
    ///   — the documented "client is older than the window" case, whose
    ///   only correct resolution is a caller-driven full resync;
    /// - `to_block` is beyond the window's current head.
    pub fn range(&self, from_block: u64, to_block: u64) -> Option<BlockDelta> {
        if from_block >= to_block {
            return None;
        }
        let want = to_block - from_block;
        let selected: Vec<BlockDelta> = self
            .buf
            .iter()
            .filter(|d| d.block > from_block && d.block <= to_block)
            .cloned()
            .collect();
        if selected.len() as u64 != want {
            // Either a block in the requested range aged out (or was
            // never pushed at all), or `to_block` is ahead of the
            // window's head. `coalesce`'s own internal contiguity check
            // could not by itself detect a *missing first element* (it
            // only validates gaps *within* the slice it is given), so
            // this length check is load-bearing, not redundant.
            return None;
        }
        BlockDelta::coalesce(&selected).ok()
    }

    /// Maximum number of blocks this ring retains — the value [`Self::new`]
    /// was constructed with, fixed for the ring's lifetime ([`Self::push`]
    /// evicts to keep the live count at or under this bound; nothing grows
    /// or shrinks it afterward). Lets a caller reason about how much of the
    /// retention window remains without keeping its own separately-tracked
    /// copy of the constructor argument — e.g. a `/setup` response cache
    /// deciding how long its pinned block stays safe to serve
    /// (`risepir-http`'s `NodeState::setup_bytes`, ADR-0028).
    pub const fn capacity(&self) -> usize {
        self.capacity
    }

    /// Oldest block currently retained, or `None` if the ring is empty.
    pub fn oldest(&self) -> Option<u64> {
        self.buf.front().map(|d| d.block)
    }

    /// Most recent block currently retained (the server's head as of the
    /// last [`Self::push`]), or `None` if the ring is empty.
    pub fn head(&self) -> Option<u64> {
        self.buf.back().map(|d| d.block)
    }
}

impl Default for DeltaRing {
    /// 300-block default window (see [`DEFAULT_CAPACITY`]).
    fn default() -> Self {
        Self::new(DEFAULT_CAPACITY)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn delta(block: u64) -> BlockDelta {
        // One nonzero cell per delta so pushes are individually
        // distinguishable and `coalesce` has something to sum.
        BlockDelta {
            block,
            per_segment: vec![vec![(0, vec![(0, 1)])]],
        }
    }

    #[test]
    fn empty_ring_reports_no_bounds_and_no_range() {
        let ring = DeltaRing::new(4);
        assert_eq!(ring.oldest(), None);
        assert_eq!(ring.head(), None);
        assert_eq!(ring.range(0, 1), None);
    }

    #[test]
    #[should_panic(expected = "capacity must be >= 1")]
    fn zero_capacity_panics() {
        let _ = DeltaRing::new(0);
    }

    #[test]
    fn eviction_respects_capacity() {
        let mut ring = DeltaRing::new(3);
        for b in 1..=5u64 {
            ring.push(delta(b));
        }
        // Only the last 3 pushes (3, 4, 5) survive.
        assert_eq!(ring.oldest(), Some(3));
        assert_eq!(ring.head(), Some(5));
    }

    #[test]
    fn range_inside_window_matches_manual_coalesce() {
        let mut ring = DeltaRing::new(10);
        let deltas: Vec<BlockDelta> = (1..=6u64).map(delta).collect();
        for d in &deltas {
            ring.push(d.clone());
        }
        let expected = BlockDelta::coalesce(&deltas[1..5]).unwrap(); // blocks 2..=5
        assert_eq!(ring.range(1, 5), Some(expected));
    }

    #[test]
    fn range_spanning_evicted_block_is_none() {
        let mut ring = DeltaRing::new(3);
        for b in 1..=5u64 {
            ring.push(delta(b));
        }
        // Window only holds blocks 3..=5; asking from block 1 needs the
        // now-evicted blocks 2 and 3's worth of history.
        assert_eq!(ring.range(1, 5), None);
        // But a range fully inside the retained window works.
        assert_eq!(ring.range(3, 5), BlockDelta::coalesce(&[delta(4), delta(5)]).ok());
    }

    #[test]
    fn range_ahead_of_head_is_none() {
        let mut ring = DeltaRing::new(10);
        ring.push(delta(1));
        ring.push(delta(2));
        assert_eq!(ring.range(0, 5), None, "to_block beyond head must not fabricate a result");
    }

    #[test]
    fn backwards_or_empty_range_is_none() {
        let mut ring = DeltaRing::new(10);
        ring.push(delta(1));
        assert_eq!(ring.range(3, 3), None);
        assert_eq!(ring.range(5, 3), None);
    }
}
