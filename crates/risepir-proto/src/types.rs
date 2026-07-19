//! The one interface everything Ethereum-specific lives behind
//! ([`BlockUpdate`]), and the delta types the incremental PIR pipeline
//! folds and coalesces.

use std::collections::BTreeMap;

/// `keccak256(address)` — the SCF key. Computed by callers (`risepir-feed`);
/// this crate only ever moves the 32 bytes around.
pub type AddressHash = [u8; 32];

/// A balance in wei.
pub type Balance = u128;

/// One block's worth of balance changes, in the shape every Ethereum-facing
/// producer (mock feed, Sepolia RPC, ExEx) must normalize to. Everything
/// upstream of this type is Ethereum; everything downstream is generic PIR
/// plumbing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockUpdate {
    /// Block number this update was derived from.
    pub block: u64,
    /// `(address_hash, new_balance)` pairs touched by this block. May
    /// contain duplicates if the same address changed balance more than
    /// once within the block (e.g. sender then later recipient); the last
    /// entry for a given key wins when folded into store mutations.
    pub changes: Vec<(AddressHash, Balance)>,
    /// `(address_hash, amount_wei)` beacon-chain withdrawal credits
    /// (EIP-4895). Unlike [`Self::changes`], these are **relative**: the
    /// amount is *added* to whatever the store holds for the address at
    /// apply time (after every entry of `changes` has been applied), because
    /// withdrawals appear in the block body as amounts, not post-balances,
    /// and the store is the authoritative prior value (ADR-0016 /
    /// `docs/sync.md`). Empty for feeds with no withdrawal concept (mock)
    /// and for pre-Shanghai blocks. May contain duplicates for one address
    /// (several validators withdrawing to one recipient in one block); they
    /// accumulate.
    pub credits: Vec<(AddressHash, Balance)>,
}

/// Sparse per-segment row deltas: `(row_index_within_segment, edits)` pairs,
/// where each edit is `(cell_offset_within_row, signed_delta_mod_2^plaintext_bits)`.
///
/// Shape-identical to `ikpir_common::wire::SegmentRowDeltas` by construction
/// (this crate does not depend on `ikpir_common`'s wire types so that
/// `risepir-proto` stays a pure-math crate; `risepir-server` is the layer
/// that owns the conversion between the two).
pub type SegmentRowDeltas = Vec<(u32, Vec<(u16, i64)>)>;

/// The sparse cell-level effect of one block on every PIR segment, indexed
/// `per_segment[j]` for segment `j` (`j` in `0..arity`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockDelta {
    /// Block this delta was produced by (single-block case) or the last
    /// block folded into it (coalesced case) — see [`BlockDelta::coalesce`].
    pub block: u64,
    /// One [`SegmentRowDeltas`] per segment; length equals the deployment's
    /// `arity`.
    pub per_segment: Vec<SegmentRowDeltas>,
}

/// Errors from [`BlockDelta::coalesce`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CoalesceError {
    /// `coalesce` was called with an empty slice; there is no `block` to
    /// report for an empty result.
    Empty,
    /// Input blocks were not the contiguous ascending run
    /// `first.block, first.block + 1, ..., last.block` — coalescing a delta
    /// with a gap would silently drop whatever changed in the missing
    /// block, which is exactly the silent-wrong-answer failure mode this
    /// type exists to prevent.
    NonContiguous {
        /// The block number that was expected at this position.
        expected: u64,
        /// The block number actually found.
        found: u64,
    },
    /// Two inputs disagreed on segment count (i.e. on `arity`), which can
    /// only happen if deltas from different deployments were mixed.
    SegmentCountMismatch {
        /// Segment count of the first input.
        expected: usize,
        /// Segment count of the offending input.
        found: usize,
    },
    /// A per-cell running sum overflowed `i64`. Not reachable with real PIR
    /// cell deltas (bounded by the plaintext modulus, `< 2^31`), but
    /// checked rather than assumed.
    Overflow,
}

impl std::fmt::Display for CoalesceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => write!(f, "coalesce called with no inputs"),
            Self::NonContiguous { expected, found } => write!(
                f,
                "non-contiguous block sequence: expected block {expected}, found {found}"
            ),
            Self::SegmentCountMismatch { expected, found } => write!(
                f,
                "segment count mismatch: expected {expected} segments, found {found}"
            ),
            Self::Overflow => write!(f, "coalesced delta overflowed i64"),
        }
    }
}

impl std::error::Error for CoalesceError {}

impl BlockDelta {
    /// Folds a contiguous, ascending run of per-block deltas into one delta
    /// summed per `(segment, row, offset)`. Zero-valued cells (and rows left
    /// with no cells) are dropped from the output; surviving rows and
    /// offsets are ascending (a [`BTreeMap`] does the sorting).
    ///
    /// The result's `block` is the **last** input's block — the coalesced
    /// delta represents "how do I get from the state as of
    /// `deltas[0].block - 1` to the state as of `deltas.last().block`".
    ///
    /// # Errors
    ///
    /// [`CoalesceError::Empty`] if `deltas` is empty.
    /// [`CoalesceError::NonContiguous`] if the blocks are not exactly
    /// `deltas[0].block, deltas[0].block + 1, ..., deltas[last].block` in
    /// order — this also catches unsorted and duplicate input.
    /// [`CoalesceError::SegmentCountMismatch`] if inputs disagree on the
    /// number of segments. [`CoalesceError::Overflow`] on `i64` overflow
    /// while summing (see the variant's doc).
    pub fn coalesce(deltas: &[BlockDelta]) -> Result<BlockDelta, CoalesceError> {
        let first = deltas.first().ok_or(CoalesceError::Empty)?;
        let num_segments = first.per_segment.len();

        for (expected, d) in (first.block..).zip(deltas.iter()) {
            if d.block != expected {
                return Err(CoalesceError::NonContiguous {
                    expected,
                    found: d.block,
                });
            }
            if d.per_segment.len() != num_segments {
                return Err(CoalesceError::SegmentCountMismatch {
                    expected: num_segments,
                    found: d.per_segment.len(),
                });
            }
        }

        let mut acc: Vec<BTreeMap<u32, BTreeMap<u16, i64>>> = vec![BTreeMap::new(); num_segments];
        for d in deltas {
            for (seg_idx, seg) in d.per_segment.iter().enumerate() {
                for (row, cells) in seg {
                    let row_map = acc[seg_idx].entry(*row).or_default();
                    for (offset, delta) in cells {
                        let slot = row_map.entry(*offset).or_insert(0i64);
                        *slot = slot.checked_add(*delta).ok_or(CoalesceError::Overflow)?;
                    }
                }
            }
        }

        let per_segment = acc
            .into_iter()
            .map(|row_map| {
                row_map
                    .into_iter()
                    .filter_map(|(row, cell_map)| {
                        let cells: Vec<(u16, i64)> =
                            cell_map.into_iter().filter(|(_, delta)| *delta != 0).collect();
                        if cells.is_empty() {
                            None
                        } else {
                            Some((row, cells))
                        }
                    })
                    .collect::<SegmentRowDeltas>()
            })
            .collect();

        Ok(BlockDelta {
            block: deltas.last().expect("checked non-empty above").block,
            per_segment,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn delta(block: u64, per_segment: Vec<SegmentRowDeltas>) -> BlockDelta {
        BlockDelta { block, per_segment }
    }

    #[test]
    fn coalesce_empty_errors() {
        assert_eq!(BlockDelta::coalesce(&[]), Err(CoalesceError::Empty));
    }

    #[test]
    fn coalesce_single_block_is_identity_modulo_canonicalization() {
        let d = delta(10, vec![vec![(2, vec![(5, 3)])]]);
        let out = BlockDelta::coalesce(std::slice::from_ref(&d)).unwrap();
        assert_eq!(out, d);
    }

    #[test]
    fn coalesce_sums_and_drops_zeros() {
        // Segment 0: row 2 offset 5 goes +3 then -3 (net zero, dropped);
        // row 2 offset 7 goes +1 then +1 (net 2, kept).
        let d1 = delta(10, vec![vec![(2, vec![(5, 3), (7, 1)])]]);
        let d2 = delta(11, vec![vec![(2, vec![(5, -3), (7, 1)])]]);
        let out = BlockDelta::coalesce(&[d1, d2]).unwrap();
        assert_eq!(out.block, 11);
        assert_eq!(out.per_segment, vec![vec![(2, vec![(7, 2)])]]);
    }

    #[test]
    fn coalesce_sorts_rows_and_offsets_ascending() {
        let d1 = delta(1, vec![vec![(9, vec![(3, 1)]), (1, vec![(9, 1), (2, 1)])]]);
        let out = BlockDelta::coalesce(std::slice::from_ref(&d1)).unwrap();
        let rows: Vec<u32> = out.per_segment[0].iter().map(|(r, _)| *r).collect();
        assert_eq!(rows, vec![1, 9]);
        let offsets_row1: Vec<u16> = out.per_segment[0][0].1.iter().map(|(o, _)| *o).collect();
        assert_eq!(offsets_row1, vec![2, 9]);
    }

    #[test]
    fn coalesce_rejects_gap() {
        let d1 = delta(10, vec![vec![]]);
        let d2 = delta(12, vec![vec![]]);
        assert_eq!(
            BlockDelta::coalesce(&[d1, d2]),
            Err(CoalesceError::NonContiguous {
                expected: 11,
                found: 12
            })
        );
    }

    #[test]
    fn coalesce_rejects_unsorted() {
        let d1 = delta(10, vec![vec![]]);
        let d2 = delta(9, vec![vec![]]);
        assert_eq!(
            BlockDelta::coalesce(&[d1, d2]),
            Err(CoalesceError::NonContiguous {
                expected: 11,
                found: 9
            })
        );
    }

    #[test]
    fn coalesce_rejects_duplicate_block() {
        let d1 = delta(10, vec![vec![]]);
        let d2 = delta(10, vec![vec![]]);
        assert_eq!(
            BlockDelta::coalesce(&[d1, d2]),
            Err(CoalesceError::NonContiguous {
                expected: 11,
                found: 10
            })
        );
    }

    #[test]
    fn coalesce_rejects_segment_count_mismatch() {
        let d1 = delta(10, vec![vec![], vec![]]);
        let d2 = delta(11, vec![vec![]]);
        assert_eq!(
            BlockDelta::coalesce(&[d1, d2]),
            Err(CoalesceError::SegmentCountMismatch {
                expected: 2,
                found: 1
            })
        );
    }

    proptest::proptest! {
        /// Coalescing a long contiguous chain of single-cell +1 deltas at
        /// the same `(segment, row, offset)` sums to the chain length —
        /// the telescoping property `docs/verification.md` relies on to
        /// bound coalesced-delta magnitude by the plaintext modulus.
        #[test]
        fn coalesce_telescopes(len in 1u32..200) {
            let deltas: Vec<BlockDelta> = (0..len)
                .map(|i| delta(u64::from(i), vec![vec![(0, vec![(0, 1)])]]))
                .collect();
            let out = BlockDelta::coalesce(&deltas).unwrap();
            assert_eq!(out.per_segment, vec![vec![(0, vec![(0, i64::from(len))])]]);
            assert_eq!(out.block, u64::from(len - 1));
        }
    }
}
