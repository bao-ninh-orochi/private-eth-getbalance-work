//! Slot mutations → sparse per-segment row deltas.
//!
//! # Purpose
//!
//! `ikpir-server`'s own `hint_patch::fold_mutations_into_row_deltas` does
//! exactly this job, but `mod hint_patch` is crate-private there, so it
//! is not reachable from outside that crate (`docs/verification.md`
//! Correction 3). This is a from-scratch reimplementation over
//! `segmented_cuckoo`'s public types (`SlotMutation`, `pack_slot_cells`,
//! `CuckooParams`) — not a copy of private source, since none was
//! available to copy from outside the crate. It is exercised against a
//! real `ikpir_server::IkpirServer` (an independent, external
//! implementation) in `server`'s `batched_equals_per_mutation` test to
//! confirm the two produce identical output.
//!
//! # Why `risepir-server` needs its own copy
//!
//! `RisePirServer::apply_block` drains the mutation log **once per
//! block** (not once per mutation, as `IkpirServer::insert/update/delete`
//! do via their own private call to this same algorithm), so it needs
//! this folding step available as a plain function it can call once over
//! however many mutations a whole block produced.
//!
//! # Algorithm
//!
//! Two passes:
//!
//! 1. **Accumulate.** Walk `muts` once, pack each `(fingerprint, value)`
//!    pair via [`pack_slot_cells`], and sum the per-cell delta
//!    (`new − old`) into a per-segment `BTreeMap<row, BTreeMap<offset,
//!    i64>>` accumulator. Multiple mutations landing on the same
//!    `(segment, row, offset)` — e.g. an insert immediately followed by
//!    an update to the same slot within one block — sum naturally.
//! 2. **Filter and emit.** Materialise the accumulator into the sparse
//!    wire shape, dropping zero net deltas and rows left with no cells so
//!    the output stays sparse.
//!
//! `BTreeMap` (not `HashMap`) so emitted rows and offsets come out sorted
//! ascending — deterministic on-wire output, and exactly the canonical
//! form [`risepir_proto::BlockDelta::coalesce`] also produces, which is
//! what makes the two directly comparable in tests.

use std::collections::BTreeMap;

use segmented_cuckoo::{pack_slot_cells, CuckooParams, SlotMutation};

use risepir_proto::SegmentRowDeltas;

/// Fold a batch of slot-level mutations into per-segment, per-row sparse
/// cell deltas suitable for
/// [`ikpir_common::IncrementalPirBackend::server_patch_hint`].
///
/// # Arguments
///
/// - `muts`   — slot mutations drained from the SCF mutation log (see
///   [`segmented_cuckoo::CuckooKVStore::drain_mutations`]).
/// - `params` — the store's current geometry; supplies arity, segment
///   size, and `cells_per_slot` (`pack_slot_cells` reuses its bit-layout
///   constants).
///
/// # Returns
///
/// `Vec<SegmentRowDeltas>` of length `params.arity()`. `out[seg]` is the
/// sparse edit list `Vec<(row_in_segment, Vec<(cell_offset_in_row,
/// delta)>)>` for segment `seg`, rows and offsets ascending. Deltas on
/// the same `(row, offset)` are summed; zero net deltas and rows left
/// empty are dropped before emission — `muts` that cancel out (e.g. an
/// insert immediately deleted within the same block) contribute nothing
/// to the output.
///
/// # Complexity
///
/// `O(|muts| · cells_per_slot)` for the accumulation pass, plus
/// `O(touched_rows · log + touched_cells · log)` for the `BTreeMap`
/// bookkeeping — one pass per block, not one pass per mutation.
pub(crate) fn fold_mutations_into_row_deltas(
    muts: &[SlotMutation],
    params: &CuckooParams,
) -> Vec<SegmentRowDeltas> {
    let arity = params.arity();
    let segment_size = params.segment_size();
    let bucket_size = params.bucket_size;
    let cps = params.cells_per_slot() as usize;

    let mut acc: Vec<BTreeMap<u32, BTreeMap<u16, i64>>> =
        (0..arity).map(|_| BTreeMap::new()).collect();

    let mut old_buf = vec![0u32; cps];
    let mut new_buf = vec![0u32; cps];

    for m in muts {
        let seg_idx = (m.bucket / segment_size) as usize;
        let bucket_in_seg = m.bucket % segment_size;
        let slot = m.slot as usize;
        debug_assert!(
            seg_idx < arity,
            "bucket {} out of range for arity {arity}",
            m.bucket
        );
        debug_assert!(
            (slot as u32) < bucket_size,
            "slot {} out of range for bucket_size {bucket_size}",
            m.slot
        );

        pack_slot_cells(params, m.old_fingerprint, &m.old_value_cells, &mut old_buf);
        pack_slot_cells(params, m.new_fingerprint, &m.new_value_cells, &mut new_buf);

        let row_entry = acc[seg_idx].entry(bucket_in_seg).or_default();
        for c in 0..cps {
            let delta = i64::from(new_buf[c]) - i64::from(old_buf[c]);
            if delta == 0 {
                continue;
            }
            let cell_offset: u16 = u16::try_from(slot * cps + c).expect(
                "row width (bucket_size * cells_per_slot) must fit u16 — the same \
                 assumption segmented_cuckoo's own wire types make",
            );
            *row_entry.entry(cell_offset).or_insert(0) += delta;
        }
    }

    acc.into_iter()
        .map(|seg| {
            seg.into_iter()
                .filter_map(|(row, cells)| {
                    let v: Vec<(u16, i64)> = cells.into_iter().filter(|(_, d)| *d != 0).collect();
                    if v.is_empty() {
                        None
                    } else {
                        Some((row, v))
                    }
                })
                .collect()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    //! Sanity tests for this reimplementation, mirroring the coverage of
    //! `ikpir-server`'s private `hint_patch` tests (segment/row routing,
    //! insert/delete duality, mutation summation, zero-pruning) so this
    //! from-scratch port is pinned independently of the
    //! `batched_equals_per_mutation` cross-crate comparison in `server`.

    use super::*;
    use segmented_cuckoo::Segmented2aryCuckooKVStore;

    /// Tiny 2-ary fixture: 64 buckets (32/segment), `bucket_size = 4`,
    /// `fingerprint_bits = 12`, `value_bits = 8`, `plaintext_bits = 8` ⇒
    /// `cells_per_slot = ⌈20/8⌉ = 3`, `value_size_in_cells = 1`.
    fn params_2ary() -> CuckooParams {
        Segmented2aryCuckooKVStore::new(64, 4, 12, 8, 8)
            .unwrap()
            .params()
    }

    fn zero_value_cells(params: &CuckooParams) -> Box<[u32]> {
        vec![0u32; params.value_size_in_cells() as usize].into_boxed_slice()
    }

    fn nonzero_value_cells(params: &CuckooParams, val: u32) -> Box<[u32]> {
        let mut v = vec![0u32; params.value_size_in_cells() as usize];
        v[0] = val;
        v.into_boxed_slice()
    }

    /// A mutation on bucket 33 (segment 1, bucket-in-seg 1) must land in
    /// `out[1]` only, at row 1.
    #[test]
    fn single_insert_lands_in_correct_segment() {
        let p = params_2ary();
        let m = SlotMutation {
            bucket: 33,
            slot: 0,
            old_fingerprint: 0,
            new_fingerprint: 7,
            old_value_cells: zero_value_cells(&p),
            new_value_cells: nonzero_value_cells(&p, 42),
        };
        let out = fold_mutations_into_row_deltas(&[m], &p);
        assert!(out[0].is_empty(), "seg 0 must be untouched");
        assert_eq!(out[1].len(), 1);
        assert_eq!(out[1][0].0, 1, "bucket_in_seg == 1");
    }

    /// Delete must produce the exact cell-by-cell negation of the
    /// corresponding insert's deltas.
    #[test]
    fn single_delete_produces_negated_deltas() {
        let p = params_2ary();
        let insert_m = SlotMutation {
            bucket: 5,
            slot: 1,
            old_fingerprint: 0,
            new_fingerprint: 99,
            old_value_cells: zero_value_cells(&p),
            new_value_cells: nonzero_value_cells(&p, 77),
        };
        let delete_m = SlotMutation {
            bucket: 5,
            slot: 1,
            old_fingerprint: 99,
            new_fingerprint: 0,
            old_value_cells: nonzero_value_cells(&p, 77),
            new_value_cells: zero_value_cells(&p),
        };
        let ins = fold_mutations_into_row_deltas(&[insert_m], &p);
        let del = fold_mutations_into_row_deltas(&[delete_m], &p);
        assert!(!ins[0].is_empty());
        assert_eq!(ins[0].len(), del[0].len());
        for ((ir, ic), (dr, dc)) in ins[0].iter().zip(del[0].iter()) {
            assert_eq!(ir, dr);
            assert_eq!(ic.len(), dc.len());
            for ((io, id), (d_o, d_d)) in ic.iter().zip(dc.iter()) {
                assert_eq!(io, d_o);
                assert_eq!(*id, -*d_d, "delete delta must negate insert delta");
            }
        }
    }

    /// insert(val=10) then update(val=10→20) must fold to the same net
    /// delta as a single insert(val=20) from empty.
    #[test]
    fn two_mutations_same_slot_sum_correctly() {
        let p = params_2ary();
        let insert_m = SlotMutation {
            bucket: 10,
            slot: 2,
            old_fingerprint: 0,
            new_fingerprint: 3,
            old_value_cells: zero_value_cells(&p),
            new_value_cells: nonzero_value_cells(&p, 10),
        };
        let update_m = SlotMutation {
            bucket: 10,
            slot: 2,
            old_fingerprint: 3,
            new_fingerprint: 3,
            old_value_cells: nonzero_value_cells(&p, 10),
            new_value_cells: nonzero_value_cells(&p, 20),
        };
        let net_expected = SlotMutation {
            bucket: 10,
            slot: 2,
            old_fingerprint: 0,
            new_fingerprint: 3,
            old_value_cells: zero_value_cells(&p),
            new_value_cells: nonzero_value_cells(&p, 20),
        };
        let chained = fold_mutations_into_row_deltas(&[insert_m, update_m], &p);
        let expected = fold_mutations_into_row_deltas(&[net_expected], &p);
        assert_eq!(
            chained, expected,
            "chained insert+update must equal single-step net delta"
        );
    }

    /// A no-op mutation (old == new, as would come from re-writing the
    /// same balance) must emit nothing — the zero-delta filter drops the
    /// row entirely.
    #[test]
    fn noop_mutation_emits_nothing() {
        let p = params_2ary();
        let m = SlotMutation {
            bucket: 7,
            slot: 0,
            old_fingerprint: 5,
            new_fingerprint: 5,
            old_value_cells: nonzero_value_cells(&p, 99),
            new_value_cells: nonzero_value_cells(&p, 99),
        };
        let out = fold_mutations_into_row_deltas(&[m], &p);
        assert!(
            out.iter().all(|seg| seg.is_empty()),
            "no-op mutation must produce empty output"
        );
    }

    /// Mutations on different buckets within the same segment land in
    /// distinct row entries, sorted by row index ascending.
    #[test]
    fn multiple_buckets_same_segment_preserve_sparsity() {
        let p = params_2ary();
        let m1 = SlotMutation {
            bucket: 2,
            slot: 0,
            old_fingerprint: 0,
            new_fingerprint: 11,
            old_value_cells: zero_value_cells(&p),
            new_value_cells: nonzero_value_cells(&p, 1),
        };
        let m2 = SlotMutation {
            bucket: 5,
            slot: 1,
            old_fingerprint: 0,
            new_fingerprint: 22,
            old_value_cells: zero_value_cells(&p),
            new_value_cells: nonzero_value_cells(&p, 2),
        };
        let out = fold_mutations_into_row_deltas(&[m1, m2], &p);
        assert!(out[1].is_empty(), "seg 1 untouched");
        assert_eq!(out[0].len(), 2, "two rows in seg 0");
        assert_eq!(out[0][0].0, 2);
        assert_eq!(out[0][1].0, 5);
    }
}
