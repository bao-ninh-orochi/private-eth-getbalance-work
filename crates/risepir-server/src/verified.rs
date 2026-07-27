//! Verified (fp **and** `key_tag`) candidate-bucket scans over a
//! `CuckooKVStore` — the server-side twin of the client's joint-mask scan
//! (ADR-0009), closing the fp-only-collision wrong-answer class in the
//! store's key-addressed write path (ADR-0017).
//!
//! # The hazard this module exists for
//!
//! `CuckooKVStore::{get, update, delete}` match a slot on the **32-bit
//! positioning fingerprint alone, first match in probe order wins**
//! (candidate buckets in order `0..arity`, slots ascending within each
//! bucket — pinned by this module's collision tests against the real
//! store). The live
//! feed routinely emits `(addr, 0)` for accounts that are *not* in the
//! store (an account touched by a block while its balance stays zero), and
//! `apply_block` maps balance `0` to a delete. With fp-only matching, such
//! a delete has a ~`arity · bucket_size · 2⁻³²` chance of hitting a
//! *foreign* account's slot that merely collides on the fingerprint —
//! silently destroying that account's entry, which then reads back as
//! `0x0`: a silent wrong answer, roughly once a year at mainnet change
//! rates. The same class affects `update` (overwriting a colliding
//! foreign entry) and `get` (reading one).
//!
//! # What this module does about it
//!
//! [`locate`] re-derives the candidate buckets and scans **every** slot in
//! them, classifying the key's presence using the same 64-bit-effective
//! joint test the client uses — SCF fingerprint *and* the value's embedded
//! `key_tag` — and, crucially, reporting *where* the key's own entry sits
//! relative to upstream's first-fp-match probe order:
//!
//! - [`Located::Own`]: the key's entry is itself the first fp-match, so
//!   the key-addressed `update`/`delete` will act on exactly that slot —
//!   safe to call.
//! - [`Located::Absent`]: no slot carries this key's fp+tag. A delete must
//!   become a **no-op** (upstream `delete` would have destroyed whatever
//!   foreign slot fp-matched!), and an update falls through to `insert`
//!   (which only ever writes *empty* slots and so cannot corrupt anyone).
//! - [`Located::Shadowed`]: the key's entry exists but a foreign
//!   fp-colliding slot precedes it in probe order — the key-addressed ops
//!   would corrupt the foreign entry. There is no safe key-addressed way
//!   through (the store's public API has no slot-addressed, log-visible
//!   write), so the caller must fail **loudly** (`docs/plan.md`'s
//!   invariant: erroring is fine; silence is not).
//! - [`Located::DuplicateTag`]: two slots match fp *and* tag (a ~2⁻⁶⁰
//!   accident or an ingest bug). Structurally ambiguous — fail loudly.
//! - [`Located::Corrupt`]: the key's entry was found but its balance
//!   failed the value checksum — the cells themselves are damaged; fail
//!   loudly, never serve from a store known to be corrupt.
//!
//! # Cost
//!
//! One extra `O(arity · bucket_size · cells_per_slot)` cell scan per
//! mutation — the same order as the probe the store op itself performs;
//! immeasurable next to the per-block hint patch.

use risepir_proto::{AddressHash, Balance, Lookup, ValueCodec};
use segmented_cuckoo::{unpack_slot_cells, CuckooKVStore, IndexScheme, SchemeMeta};

/// Outcome of a verified candidate-bucket scan for one key. See the
/// module docs for what each variant obliges the caller to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Located {
    /// The key's own entry (fp **and** `key_tag` match) is the first
    /// fp-match in upstream probe order; the balance decoded clean.
    /// Key-addressed `update`/`delete` will act on exactly this slot.
    Own(Balance),
    /// No slot in the candidate set carries this key's fp+tag.
    /// `foreign_fp_matches` counts slots that fp-match anyway — each one
    /// is a slot upstream's key-addressed ops would have wrongly acted on.
    Absent {
        /// Number of fp-matching slots belonging to *other* keys.
        foreign_fp_matches: u32,
    },
    /// The key's entry exists but a foreign fp-colliding slot precedes it
    /// in probe order. Key-addressed ops would corrupt the foreign entry.
    Shadowed,
    /// More than one slot matched fp **and** tag.
    DuplicateTag,
    /// The key's entry was found (fp+tag) but its balance failed the
    /// checksum decode — the store's cells are corrupt.
    Corrupt,
}

/// Scan every slot of `key`'s candidate buckets and classify its presence.
///
/// Probe order mirrors `CuckooKVStore::{get, update, delete}` exactly:
/// candidate buckets in `candidate_buckets` order (`0..arity`), slots
/// ascending within each bucket, and "first fp-match" means the first slot
/// in that order whose fingerprint equals the key's — pinned against the
/// real store by this module's collision tests.
pub(crate) fn locate<S: IndexScheme + SchemeMeta>(
    store: &CuckooKVStore<S>,
    codec: &ValueCodec,
    key: &AddressHash,
) -> Located {
    let params = store.params();
    let (fp, indices) = params.candidate_buckets(key);
    let arity = params.arity();
    let bucket_size = params.bucket_size;
    let cells = store.as_cells();
    let tag = codec.key_tag(key);

    let mut foreign_fp_matches = 0u32;
    let mut foreign_seen_before_ours = false;
    let mut ours: Option<Lookup> = None;

    for &bucket in indices.iter().take(arity) {
        for slot in 0..bucket_size {
            let range = params.slot_cell_range(bucket, slot);
            let (slot_fp, value_bytes) = unpack_slot_cells(&params, &cells[range]);
            if slot_fp != fp {
                continue; // empty (fp 0) or unrelated occupant
            }
            if codec.extract_key_tag(&value_bytes) == tag {
                if ours.is_some() {
                    return Located::DuplicateTag;
                }
                if foreign_fp_matches > 0 {
                    foreign_seen_before_ours = true;
                }
                ours = Some(codec.decode(key, &value_bytes));
            } else {
                foreign_fp_matches += 1;
            }
        }
    }

    match ours {
        None => Located::Absent { foreign_fp_matches },
        Some(_) if foreign_seen_before_ours => Located::Shadowed,
        Some(Lookup::Found(balance)) => Located::Own(balance),
        // `decode` returning `NotFound` here is impossible (the tag just
        // matched); fold it into `Corrupt` rather than trusting that
        // reasoning with a panic on live data.
        Some(Lookup::NotFound | Lookup::DecodeFailed) => Located::Corrupt,
    }
}

/// Verified read: the balance stored for `key`, or `None` if absent —
/// never a colliding foreign entry's bytes (which is what
/// `CuckooKVStore::get`'s fp-only first-match can return).
///
/// # Errors
///
/// The [`Located`] ambiguity/corruption variant the caller must treat as
/// fatal ([`Located::DuplicateTag`] / [`Located::Corrupt`]); never
/// [`Located::Own`] / [`Located::Absent`] / [`Located::Shadowed`].
///
/// Unlike [`locate`], probe *position* is irrelevant for a pure read; only
/// existence and uniqueness of the fp+tag match matter. `Err` carries the
/// ambiguity/corruption cases the caller must treat as fatal.
pub(crate) fn get<S: IndexScheme + SchemeMeta>(
    store: &CuckooKVStore<S>,
    codec: &ValueCodec,
    key: &AddressHash,
) -> Result<Option<Balance>, Located> {
    match locate(store, codec, key) {
        Located::Own(b) => Ok(Some(b)),
        // Shadowed is fine for a READ: the key's entry exists and is
        // unique on fp+tag; only key-addressed WRITES care about probe
        // position. Re-scan for the unique tagged slot's balance.
        Located::Shadowed => {
            let params = store.params();
            let (fp, indices) = params.candidate_buckets(key);
            let cells = store.as_cells();
            let tag = codec.key_tag(key);
            for &bucket in indices.iter().take(params.arity()) {
                for slot in 0..params.bucket_size {
                    let range = params.slot_cell_range(bucket, slot);
                    let (slot_fp, value_bytes) = unpack_slot_cells(&params, &cells[range]);
                    if slot_fp == fp && codec.extract_key_tag(&value_bytes) == tag {
                        return match codec.decode(key, &value_bytes) {
                            Lookup::Found(b) => Ok(Some(b)),
                            _ => Err(Located::Corrupt),
                        };
                    }
                }
            }
            // Unreachable: locate just saw the entry. Treat as corrupt
            // rather than panicking on live data.
            Err(Located::Corrupt)
        }
        Located::Absent { .. } => Ok(None),
        err @ (Located::DuplicateTag | Located::Corrupt) => Err(err),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ikpir_common::backend::simple::SimpleParams;
    use ikpir_common::pir_params::simple_max_plaintext_bits;
    use segmented_cuckoo::{CuckooParams, Segmented2aryCuckooKVStore};
    use std::collections::HashMap;

    // Tiny store so the birthday search below only needs ~2^17.7 keys:
    // colliding on (32-bit fp, first candidate bucket) is a 2^35 space at
    // segment_size 8.
    const NUM_BUCKETS: u32 = 2 * 8;
    const BUCKET_SIZE: u32 = 4;
    const FP_BITS: u32 = 32;

    fn codec() -> ValueCodec {
        ValueCodec {
            key_tag_bits: 32,
            balance_bits: 96,
            checksum_bits: 16,
        }
    }

    fn store() -> Segmented2aryCuckooKVStore {
        let value_bits = codec().value_bits();
        let pb = simple_max_plaintext_bits(NUM_BUCKETS / 2, BUCKET_SIZE, FP_BITS, value_bits, SimpleParams::DEFAULT_SIGMA);
        Segmented2aryCuckooKVStore::new(NUM_BUCKETS, BUCKET_SIZE, FP_BITS, value_bits, pb).unwrap()
    }

    fn addr(i: u64) -> AddressHash {
        let mut a = [0u8; 32];
        a[..8].copy_from_slice(&i.to_le_bytes());
        a
    }

    /// Deterministic birthday search: the first two distinct addresses
    /// (by the `addr(i)` enumeration) that collide on **both** the 32-bit
    /// SCF fingerprint and their first candidate bucket. Returned in
    /// enumeration order, so every test that calls this gets the same
    /// pair — the collision scenario is pinned, not sampled.
    fn colliding_pair(params: &CuckooParams) -> (AddressHash, AddressHash) {
        let mut seen: HashMap<(u32, u32), u64> = HashMap::new();
        for i in 0u64..30_000_000 {
            let a = addr(i);
            let (fp, idx) = params.candidate_buckets(&a);
            if let Some(&j) = seen.get(&(fp, idx[0])) {
                return (addr(j), a);
            }
            seen.insert((fp, idx[0]), i);
        }
        panic!("no (fingerprint, first-bucket) collision found in 30M keys — should be ~2^18");
    }

    /// The hazard is real at the store level, and the verified scan closes
    /// it. With only B present: upstream `get(A)` returns **B's** value
    /// bytes (fp-only first match), and upstream `delete(A)` **destroys
    /// B's entry** — while `verified::get(A)` correctly answers `None` and
    /// `locate(A)` reports `Absent` with one foreign fp-match.
    #[test]
    fn upstream_fp_only_ops_corrupt_a_colliding_foreign_entry_verified_ops_do_not() {
        let codec = codec();
        let mut st = store();
        let params = st.params();
        let (a, b) = colliding_pair(&params);
        let (fp_a, idx_a) = params.candidate_buckets(&a);
        let (fp_b, idx_b) = params.candidate_buckets(&b);
        assert_eq!(fp_a, fp_b, "search invariant: same fingerprint");
        assert_eq!(idx_a[0], idx_b[0], "search invariant: same first candidate bucket");
        assert_ne!(a, b);

        let b_balance: Balance = 42_000_000_000_000_000_000u128;
        st.insert(b, &codec.encode(&b, b_balance).unwrap()).unwrap();

        // Upstream get(A): fp-only first match returns B's bytes — the
        // misread the value-side key_tag exists to catch.
        let raw = st.get(a).expect("upstream get(A) fp-matches B's slot");
        assert_eq!(
            codec.decode(&a, &raw),
            risepir_proto::Lookup::NotFound,
            "B's bytes must not decode as A's (key_tag catches the misread)"
        );

        // Verified read/locate: A is absent, with exactly one foreign match.
        assert_eq!(get(&st, &codec, &a), Ok(None));
        assert_eq!(locate(&st, &codec, &a), Located::Absent { foreign_fp_matches: 1 });
        assert_eq!(get(&st, &codec, &b), Ok(Some(b_balance)));

        // Upstream delete(A) — what apply_block used to do on (A, 0) —
        // destroys B's entry.
        st.delete(a).expect("upstream delete(A) hits B's slot");
        assert_eq!(
            get(&st, &codec, &b),
            Ok(None),
            "pinned hazard: fp-only delete of absent A destroyed B's entry"
        );
    }

    /// Probe-position classification: with B first in probe order and A's
    /// own entry behind it, `locate(A)` must say `Shadowed` (key-addressed
    /// writes unsafe) while `locate(B)` stays `Own`, and the verified read
    /// still disambiguates both balances correctly.
    #[test]
    fn locate_classifies_own_shadowed_and_reads_disambiguate() {
        let codec = codec();
        let mut st = store();
        let params = st.params();
        let (a, b) = colliding_pair(&params);

        let b_balance: Balance = 7_000_000_000u128;
        let a_balance: Balance = 9_999_999_999u128;
        // Empty store: B lands in the shared first bucket, slot 0; A's
        // insert then takes slot 1 of the same bucket (first empty slot,
        // ascending — the upstream placement order this module mirrors).
        st.insert(b, &codec.encode(&b, b_balance).unwrap()).unwrap();
        st.insert(a, &codec.encode(&a, a_balance).unwrap()).unwrap();

        assert_eq!(locate(&st, &codec, &b), Located::Own(b_balance));
        assert_eq!(locate(&st, &codec, &a), Located::Shadowed);

        // Reads are position-independent: both resolve correctly.
        assert_eq!(get(&st, &codec, &a), Ok(Some(a_balance)));
        assert_eq!(get(&st, &codec, &b), Ok(Some(b_balance)));

        // Delete B (safe: Own) — A un-shadows and becomes Own.
        st.delete(b).unwrap();
        assert_eq!(locate(&st, &codec, &a), Located::Own(a_balance));
        assert_eq!(get(&st, &codec, &b), Ok(None));
    }

    /// Non-colliding keys: locate/get behave as plain presence checks.
    #[test]
    fn locate_and_get_on_plain_keys() {
        let codec = codec();
        let mut st = store();
        let k = addr(1);
        assert_eq!(locate(&st, &codec, &k), Located::Absent { foreign_fp_matches: 0 });
        assert_eq!(get(&st, &codec, &k), Ok(None));

        st.insert(k, &codec.encode(&k, 5u128).unwrap()).unwrap();
        assert_eq!(locate(&st, &codec, &k), Located::Own(5));
        assert_eq!(get(&st, &codec, &k), Ok(Some(5)));
    }
}
