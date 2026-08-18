//! [`PendingDelta`] — the rolling public `ΔD` a [`crate::RisePirClient`]
//! accumulates since its pinned hint.

use std::collections::btree_map::Entry;
use std::collections::BTreeMap;

use risepir_proto::{BlockDelta, SegmentRowDeltas};

use crate::error::ClientError;

/// Rolling public `ΔD` accumulated since [`crate::RisePirClient`]'s
/// pinned hint, one `BTreeMap<(row, cell_offset), delta>` per PIR segment
/// (`docs/plan.md` §3.3 / §3.7).
///
/// # Purpose
///
/// The client never needs to be at the server's head: instead it pins a
/// hint at a block and rolls forward a sparse public delta that,
/// together with the pinned hint, can bit-exactly reconstruct the
/// database as of any later block the delta has been folded up to (see
/// [`Self::head`]).
///
/// # Rationale for a flat running sum, not a history
///
/// Only the *summed* effect since the pinned block is kept — not the
/// individual per-block deltas that produced it. This is sufficient for
/// both consumers ([`crate::RisePirClient::finish`]'s response rewind,
/// which needs the whole-segment sum, and
/// [`crate::RisePirClient::collect_garbage`], which folds the whole sum
/// into the hint at once) and keeps memory proportional to touched
/// cells, not blocks. The cost: garbage collection can only target
/// exactly [`Self::head`] (see that method's docs) — arbitrary partial
/// GC would require retaining per-block history, which no required use
/// case here needs.
#[derive(Debug)]
pub(crate) struct PendingDelta {
    per_segment: Vec<BTreeMap<(u32, u16), i64>>,
    head: u64,
}

impl PendingDelta {
    /// A fresh, empty accumulator representing "no change yet" as of
    /// `head` (the block the freshly pinned hint was taken at).
    pub(crate) fn new(arity: usize, head: u64) -> Self {
        Self {
            per_segment: vec![BTreeMap::new(); arity],
            head,
        }
    }

    /// Block number this accumulator currently represents state as of:
    /// the pinned hint plus everything folded in here equals the
    /// database as of this block.
    pub(crate) const fn head(&self) -> u64 {
        self.head
    }

    /// Total number of nonzero `(segment, row, offset)` cells currently
    /// pending across every segment.
    pub(crate) fn cells(&self) -> usize {
        self.per_segment.iter().map(BTreeMap::len).sum()
    }

    /// Segment `seg`'s whole accumulated delta map — every touched cell
    /// since the pinned block, anywhere in the segment. Used by the
    /// response rewind's step 2, which must correct for changes anywhere
    /// in the segment (not just the queried row): `resp -= qᵀ·ΔD`.
    pub(crate) fn segment(&self, seg: usize) -> &BTreeMap<(u32, u16), i64> {
        &self.per_segment[seg]
    }

    /// Deltas for exactly `row` in segment `seg`, ascending by offset —
    /// used by the rewind's step 4, which patches only the queried row,
    /// *after* decode and *before* the fingerprint scan.
    ///
    /// A `BTreeMap<(u32, u16), i64>` orders lexicographically by `(row,
    /// offset)`, so `(row, 0)..=(row, u16::MAX)` is exactly every entry
    /// for `row`, in ascending offset order — no secondary index needed.
    pub(crate) fn row(&self, seg: usize, row: u32) -> impl Iterator<Item = (u16, i64)> + '_ {
        self.per_segment[seg]
            .range((row, 0u16)..=(row, u16::MAX))
            .map(|(&(_, off), &d)| (off, d))
    }

    /// Materialise segment `seg`'s accumulated delta into the
    /// `SegmentRowDeltas` shape `IncrementalPirBackend::client_patch_state`
    /// expects (rows ascending, offsets ascending within each row) — used
    /// by [`crate::RisePirClient::collect_garbage`] to fold the whole
    /// accumulator into the hint in one call.
    pub(crate) fn as_row_deltas(&self, seg: usize) -> SegmentRowDeltas {
        let mut out: SegmentRowDeltas = Vec::new();
        for (&(row, off), &delta) in &self.per_segment[seg] {
            match out.last_mut() {
                Some((last_row, cells)) if *last_row == row => cells.push((off, delta)),
                _ => out.push((row, vec![(off, delta)])),
            }
        }
        out
    }

    /// Fold one more delta into this accumulator — either a genuine
    /// single-block [`BlockDelta`] or a caller-pre-coalesced span of
    /// several (`docs/plan.md` §3.4's `sync`).
    ///
    /// # Errors
    ///
    /// [`ClientError::NonContiguousDelta`] if `d.block <= self.head()` —
    /// the only shape of "does not extend the chain forward" this type
    /// can detect (see that variant's docs for the honest limits of the
    /// check). [`ClientError::SegmentCountMismatch`] if
    /// `d.per_segment.len()` disagrees with this accumulator's arity.
    /// [`ClientError::DeltaOverflow`] on `i64` overflow while summing
    /// (mirrors [`risepir_proto::CoalesceError::Overflow`]; not reachable
    /// with real PIR cell deltas).
    pub(crate) fn ingest(&mut self, d: &BlockDelta) -> Result<(), ClientError> {
        if d.block <= self.head {
            return Err(ClientError::NonContiguousDelta {
                current_head: self.head,
                found: d.block,
            });
        }
        if d.per_segment.len() != self.per_segment.len() {
            return Err(ClientError::SegmentCountMismatch {
                expected: self.per_segment.len(),
                found: d.per_segment.len(),
            });
        }

        for (seg, rows) in d.per_segment.iter().enumerate() {
            for (row, cells) in rows {
                for (off, delta) in cells {
                    let key = (*row, *off);
                    match self.per_segment[seg].entry(key) {
                        Entry::Vacant(v) => {
                            if *delta != 0 {
                                v.insert(*delta);
                            }
                        }
                        Entry::Occupied(mut o) => {
                            let sum = o
                                .get()
                                .checked_add(*delta)
                                .ok_or(ClientError::DeltaOverflow)?;
                            if sum == 0 {
                                o.remove();
                            } else {
                                *o.get_mut() = sum;
                            }
                        }
                    }
                }
            }
        }
        self.head = d.block;
        Ok(())
    }
}
