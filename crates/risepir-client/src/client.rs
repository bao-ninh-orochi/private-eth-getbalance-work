//! [`RisePirClient`] — the response-rewind client (`docs/plan.md` §3.3,
//! ADR-0003). See the crate docs for the rewind steps in full.

use ikpir_common::{HintPatchMode, IncrementalPirBackend, IndexPirBackend};
use risepir_proto::{AddressHash, BlockDelta, Lookup, ValueCodec};
use risepir_server::SetupBundle;
use segmented_cuckoo::{unpack_slot_cells, CuckooParams};

use crate::delta::PendingDelta;
use crate::error::ClientError;
use crate::rewind::ResponseRewind;

/// Client-held bridge between [`RisePirClient::build_query`] and
/// [`RisePirClient::finish`].
///
/// Carries the exact per-segment queries [`RisePirClient::build_query`]
/// built — needed for the rewind's step 2, which depends on the query's
/// own LWE randomness and so cannot be recomputed from the key alone —
/// and the rows they targeted (cheap to recompute from the key; cached
/// here only so `finish` need not re-hash, and cross-checked there
/// against a fresh hash of `key` — see
/// [`ClientError::KeyContextMismatch`]).
pub struct QueryCtx<B: IndexPirBackend> {
    queries: Vec<B::Query>,
    rows: Vec<u32>,
}

/// The response-rewind RisePIR client.
///
/// Pins `(A, H₀, block₀)` at [`Self::from_setup`] and never needs to
/// catch up to the server's head: [`Self::finish`] corrects a response
/// answered at the server's current block against a rolling public
/// delta (`docs/plan.md` §3.3, ADR-0003). Hint patching
/// ([`Self::collect_garbage`]) is garbage collection, not
/// synchronisation — safe to run on any cadence, or never.
///
/// # The rewind, in order (see [`Self::finish`])
///
/// ```text
/// 1. resp  ← server.answer(q)              // answered at the server's head E'
/// 2. resp -= qᵀ·ΔD[block₀ → E']            // → BIT-EXACT the response a block₀ server would give
/// 3. cells ← B::client_decode(state, resp) // → the bucket AS OF block₀   (decodes vs the STALE hint)
/// 4. cells += ΔD[row]                      // → the bucket AS OF E'    ***BEFORE STEP 5***
/// 5. scan cells for fp(key) ∧ key_tag(key), jointly per slot (ADR-0009)
/// ```
///
/// **Step 4 must precede step 5.** If step 5 (the fingerprint scan) ran
/// on the still-pinned bucket, a key inserted or kicked after `block₀`
/// would have no matching fingerprint there yet, and the scan would
/// silently report [`Lookup::NotFound`] for an account that demonstrably
/// exists — the silent-wrong-answer bug `docs/verification.md`
/// reproduces and pins with a dedicated regression test
/// (`created_during_run`). [`Self::finish`] performs steps 2 through 5 in
/// exactly this order; it is not configurable.
///
/// # What this type does *not* decide
///
/// [`Lookup::NotFound`] is returned as-is, never converted to a balance
/// of zero — that policy (correct only for a *complete* nonzero-balance
/// database, ADR-0015) belongs to the caller, not this crate.
///
/// # Concurrency
///
/// The concrete backend types are auto-`Send` (`docs/plan.md` ADR-0010;
/// pinned by this crate's `send_sync` test), so `Mutex<RisePirClient<B>>`
/// is the intended way to share one client across threads — queries are
/// strictly serialised behind the mutex, matching the single-in-flight
/// contract each segment's `B::ClientState` already imposes (a second
/// [`Self::build_query`] before the matching [`Self::finish`] discards
/// the first query's secret).
pub struct RisePirClient<B: IncrementalPirBackend> {
    params: CuckooParams,
    states: Vec<B::ClientState>,
    pinned_block: u64,
    delta: PendingDelta,
    value_codec: ValueCodec,
}

impl<B: IncrementalPirBackend> RisePirClient<B> {
    /// Bootstrap a fresh client from a server's setup bundle.
    ///
    /// Builds one `B::ClientState` per segment via
    /// [`IndexPirBackend::client_setup`] and pins the hint at
    /// `bundle.block` — the client never needs to advance this until
    /// [`Self::collect_garbage`] is explicitly called.
    ///
    /// # Panics
    ///
    /// Debug builds assert `bundle.backend_params.len()` and
    /// `bundle.hints.len()` both equal `bundle.params.arity()` — a
    /// deployment-construction invariant [`risepir_server::RisePirServer::setup`]
    /// already guarantees, not a condition arising from any input this
    /// crate accepts from elsewhere.
    pub fn from_setup(bundle: SetupBundle<B>, value_codec: ValueCodec) -> Self {
        let arity = bundle.params.arity();
        debug_assert_eq!(bundle.backend_params.len(), arity);
        debug_assert_eq!(bundle.hints.len(), arity);

        let states: Vec<B::ClientState> = bundle
            .backend_params
            .iter()
            .zip(bundle.hints.iter())
            .map(|(p, h)| B::client_setup(p, h))
            .collect();

        let pinned_block = bundle.block;
        Self {
            params: bundle.params,
            states,
            pinned_block,
            delta: PendingDelta::new(arity, pinned_block),
            value_codec,
        }
    }

    /// Fold one more block's delta (or a caller-pre-coalesced span of
    /// several) into the client's rolling `ΔD`.
    ///
    /// Arbitrary gaps between *calls* are fine — the client never has to
    /// ingest every block promptly, and a single call may carry a
    /// coalesced span of many blocks at once (`docs/plan.md` §3.4's
    /// `sync`). What is rejected is a delta that does not extend the
    /// accumulator strictly forward (stale, duplicate, or
    /// out-of-order) — see [`ClientError::NonContiguousDelta`].
    ///
    /// # Errors
    ///
    /// See [`ClientError::NonContiguousDelta`] /
    /// [`ClientError::SegmentCountMismatch`] /
    /// [`ClientError::DeltaOverflow`] — [`PendingDelta::ingest`] performs
    /// every check.
    pub fn ingest_delta(&mut self, d: &BlockDelta) -> Result<(), ClientError> {
        self.delta.ingest(d)
    }

    /// Fold the *entire* currently-pending `ΔD` into every segment's
    /// hint via [`IncrementalPirBackend::client_patch_state`], and
    /// advance [`Self::pinned_block`] to `upto`.
    ///
    /// # Why `upto` must equal the pending delta's current head
    ///
    /// [`PendingDelta`] keeps only the *summed* effect since the pinned
    /// block, not the individual per-block deltas that produced it (see
    /// that type's docs) — so there is no way to fold only part of it
    /// while leaving a correct remainder pinned at some intermediate
    /// block. Folding the whole sum makes the hint correct exactly as of
    /// the pending delta's head; if `upto` were anything else,
    /// [`Self::pinned_block`] and the hint would disagree, which is
    /// exactly the kind of drift this crate exists to prevent. Passing
    /// the wrong `upto` therefore errors loudly rather than silently
    /// garbage-collecting to the wrong block.
    ///
    /// A no-op (`Ok(())`, nothing patched) when nothing is pending, i.e.
    /// `upto == self.pinned_block()`.
    ///
    /// # Errors
    ///
    /// [`ClientError::GcTargetMismatch`] if `upto` is not exactly the
    /// pending delta's current head.
    pub fn collect_garbage(&mut self, upto: u64) -> Result<(), ClientError> {
        let pending_head = self.delta.head();
        if upto != pending_head {
            return Err(ClientError::GcTargetMismatch {
                pending_head,
                requested: upto,
            });
        }
        if upto == self.pinned_block {
            return Ok(()); // nothing pending
        }
        for (j, state) in self.states.iter_mut().enumerate() {
            let row_deltas = self.delta.as_row_deltas(j);
            if !row_deltas.is_empty() {
                B::client_patch_state(state, &row_deltas, HintPatchMode::EntryLevel);
            }
        }
        self.pinned_block = upto;
        self.delta = PendingDelta::new(self.states.len(), upto);
        Ok(())
    }

    /// The block the client's hint is currently pinned at.
    pub const fn pinned_block(&self) -> u64 {
        self.pinned_block
    }

    /// Number of nonzero `(segment, row, offset)` cells currently pending
    /// in `ΔD` — drops (typically to `0`) after [`Self::collect_garbage`].
    pub fn delta_cells(&self) -> usize {
        self.delta.cells()
    }
}

impl<B: IncrementalPirBackend> RisePirClient<B>
where
    B::Query: Clone,
{
    /// Build a per-segment PIR query for `key`.
    ///
    /// Re-derives the SCF candidate buckets locally
    /// ([`CuckooParams::candidate_buckets`]) — no privacy cost, this
    /// geometry is public. Returns the query bundle to send to the
    /// server alongside a [`QueryCtx`] the client retains locally and
    /// later passes to [`Self::finish`].
    ///
    /// # Constraints
    ///
    /// **Single in-flight query per segment.** Each call overwrites the
    /// per-segment `B::ClientState`'s in-flight slot; calling this again
    /// for a different key before [`Self::finish`]-ing the first
    /// discards the first query's secret and that first `finish` will
    /// decode garbage. This matches the upstream backends' own
    /// documented contract.
    pub fn build_query(&mut self, key: &AddressHash) -> (Vec<B::Query>, QueryCtx<B>) {
        let (_fp, indices) = self.params.candidate_buckets(key);
        let arity = self.params.arity();
        let segment_size = self.params.segment_size();

        let mut to_send = Vec::with_capacity(arity);
        let mut ctx_queries = Vec::with_capacity(arity);
        let mut rows = Vec::with_capacity(arity);
        // `indices` is a fixed `[u32; 4]` with only the first `arity`
        // entries meaningful — the `0..arity` bound is load-bearing.
        #[allow(clippy::needless_range_loop)]
        for j in 0..arity {
            let row = indices[j] % segment_size;
            let q = B::client_query(&mut self.states[j], row);
            ctx_queries.push(q.clone());
            to_send.push(q);
            rows.push(row);
        }
        let ctx = QueryCtx {
            queries: ctx_queries,
            rows,
        };
        (to_send, ctx)
    }
}

impl<B: IncrementalPirBackend + ResponseRewind> RisePirClient<B> {
    /// Complete a lookup: steps 2-5 of the rewind (`docs/plan.md` §3.3),
    /// in the one order that is correct.
    ///
    /// # Arguments
    ///
    /// - `key`      — the same key passed to the matching
    ///   [`Self::build_query`].
    /// - `ctx`      — the [`QueryCtx`] that call returned.
    /// - `resp`     — the per-segment responses from `server.answer(q)`.
    /// - `at_block` — the block the server reports it answered at (the
    ///   second element of `RisePirServer::answer`'s return value).
    ///
    /// # Why `at_block` must equal the pending delta's head
    ///
    /// Step 2 needs `ΔD[block₀ → E']` — the *entire* accumulated delta
    /// from the pinned block up to the block the response was actually
    /// computed at. [`PendingDelta`] only ever holds that entire sum up
    /// to its own head; if the response were answered at some other
    /// block, the correction this client can compute would be for the
    /// wrong span, silently producing a wrong balance instead of the
    /// bit-exact identity the rewind relies on. Rather than guess, this
    /// is rejected — `docs/plan.md` §3.6: "client behind / server
    /// stalled → label the block; report stalled; never guess." A
    /// caller that keeps [`Self::ingest_delta`] current with the same
    /// server before calling `finish` will always satisfy this.
    ///
    /// # Why the fp / `key_tag` mask must be joint, per slot (ADR-0009)
    ///
    /// Step 5 selects a slot into the accumulator only when **both**
    /// `fp == fp(key)` **and** `key_tag == value_codec.key_tag(key)` hold
    /// for that *same* slot — never fp first, with `key_tag` checked only
    /// afterward on whatever got selected. The store's own fingerprint is
    /// just 32 bits, so a *different* key can collide on `fp(key)` in one
    /// candidate slot while `key`'s real entry sits in another candidate
    /// slot (same segment or a different one). Selecting on `fp` alone
    /// would OR that colliding slot's unrelated value bytes into the
    /// accumulator too — even though its `key_tag` would eventually fail
    /// a *separate* check, by then its bytes are already mixed with the
    /// real entry's, and the combined garbage decodes to neither: a
    /// present account could silently come back [`Lookup::NotFound`]
    /// (`0x0`) instead of its real balance — a ~2⁻²⁸ silent-wrong-answer,
    /// exactly the failure mode `docs/plan.md`'s invariant forbids.
    /// Masking jointly means a colliding slot's mask is all-zero, so it
    /// contributes nothing to the accumulator no matter what its bytes
    /// are.
    ///
    /// # Errors
    ///
    /// [`ClientError::MalformedResponse`] if `resp.len()` (or `ctx`'s
    /// internal lengths) does not match this deployment's arity.
    /// [`ClientError::ResponseBlockMismatch`] if `at_block` is not
    /// exactly the pending delta's current head.
    /// [`ClientError::KeyContextMismatch`] if `ctx`'s recorded rows don't
    /// match what `key` hashes to (a caller bug — mismatched `(key,
    /// ctx)` pair). [`ClientError::CellOutOfRange`] if, after step 4, a
    /// corrected cell falls outside `[0, 2^plaintext_bits)` — the free
    /// integrity check the coalesced-delta magnitude bound licenses
    /// (`docs/verification.md`); never returned for honestly-produced
    /// data.
    pub fn finish(
        &self,
        key: &AddressHash,
        ctx: &QueryCtx<B>,
        mut resp: Vec<B::Response>,
        at_block: u64,
    ) -> Result<Lookup, ClientError> {
        let arity = self.params.arity();
        if resp.len() != arity || ctx.queries.len() != arity || ctx.rows.len() != arity {
            return Err(ClientError::MalformedResponse {
                expected: arity,
                found: resp.len(),
            });
        }
        let pending_head = self.delta.head();
        if at_block != pending_head {
            return Err(ClientError::ResponseBlockMismatch {
                pending_head,
                response_block: at_block,
            });
        }

        let (fp, indices) = self.params.candidate_buckets(key);
        let segment_size = self.params.segment_size();
        // `indices` is a fixed `[u32; 4]`; only the first `arity` entries
        // are meaningful, so the `0..arity` bound is load-bearing.
        #[allow(clippy::needless_range_loop)]
        for j in 0..arity {
            if ctx.rows[j] != indices[j] % segment_size {
                return Err(ClientError::KeyContextMismatch);
            }
        }

        let bucket_size = self.params.bucket_size as usize;
        let cps = self.params.cells_per_slot() as usize;
        let value_size = self.params.value_size_in_bytes();
        let plaintext_bound: i64 = 1i64 << self.params.plaintext_bits;

        // Computed once: every candidate slot in every segment is checked
        // against this same `key_tag` (see "Why the fp / key_tag mask must
        // be joint" above).
        let key_tag = self.value_codec.key_tag(key);
        let mut acc = vec![0u8; value_size];
        let mut found_mask: u64 = 0;

        // `j` addresses four parallel per-segment structures; the indexed
        // form keeps them visibly in lockstep.
        #[allow(clippy::needless_range_loop)]
        for j in 0..arity {
            // Step 2: resp -= qᵀ·ΔD[block₀ → E'], this segment's whole delta.
            B::rewind_response(
                &self.states[j],
                &ctx.queries[j],
                &mut resp[j],
                self.delta.segment(j),
            );

            // Step 3: decode against the STALE (pinned) hint — the bucket AS OF block₀.
            let mut cells: Vec<u32> = B::client_decode(&self.states[j], &resp[j]);

            // Step 4: cells += ΔD[row] — the bucket AS OF E'. MUST precede step 5.
            let row = ctx.rows[j];
            for (offset, cell_delta) in self.delta.row(j, row) {
                let idx = offset as usize;
                if idx >= cells.len() {
                    return Err(ClientError::CellOutOfRange {
                        segment: j,
                        row,
                        offset,
                    });
                }
                let corrected = i64::from(cells[idx]) + cell_delta;
                if corrected < 0 || corrected >= plaintext_bound {
                    return Err(ClientError::CellOutOfRange {
                        segment: j,
                        row,
                        offset,
                    });
                }
                cells[idx] = corrected as u32;
            }

            // Step 5 (this segment's share): scan for fp(key) AND
            // key_tag(key), side-channel hardened exactly as
            // `IkpirClient::decode` is — a branchless OR-masked select
            // over every candidate slot, independent of where (if
            // anywhere) the match is. The two checks are ANDed into one
            // JOINT per-slot mask before selecting — see "Why the fp /
            // key_tag mask must be joint" on this method's docs; this is
            // NOT the same as selecting on fp alone and checking key_tag
            // afterward on whatever got selected.
            for s in 0..bucket_size {
                let slot = &cells[s * cps..(s + 1) * cps];
                let (decoded_fp, value_bytes) = unpack_slot_cells(&self.params, slot);
                let mask = ct_eq_u32_mask(decoded_fp, fp) as u64
                    & ct_eq_u64_mask(self.value_codec.extract_key_tag(&value_bytes), key_tag);
                let mask8 = (mask & 0xFF) as u8;
                for (a, v) in acc.iter_mut().zip(value_bytes.iter()) {
                    *a |= mask8 & *v;
                }
                found_mask |= mask;
            }
        }

        if found_mask == 0 {
            return Ok(Lookup::NotFound);
        }
        Ok(self.value_codec.decode(key, &acc))
    }
}

/// Branchless `u32` equality mask: `0xFFFF_FFFF` if `a == b`, else `0`.
/// Same trick `IkpirClient::decode` uses, so the scan stays
/// side-channel hardened in exactly the same way.
#[inline]
const fn ct_eq_u32_mask(a: u32, b: u32) -> u32 {
    let x = a ^ b;
    ((x | x.wrapping_neg()) >> 31).wrapping_sub(1)
}

/// Branchless `u64` equality mask: `0xFFFF_FFFF_FFFF_FFFF` if `a == b`,
/// else `0`. Same trick as [`ct_eq_u32_mask`], widened to 64 bits — used
/// to fold [`RisePirClient::finish`]'s per-slot `key_tag` check into the
/// same branchless select as the fingerprint check, so the two can be
/// ANDed into one joint mask (see that method's docs) without ever
/// materialising an intermediate branch on either check alone.
#[inline]
const fn ct_eq_u64_mask(a: u64, b: u64) -> u64 {
    let x = a ^ b;
    ((x | x.wrapping_neg()) >> 63).wrapping_sub(1)
}

// ─── Send + Sync static assertion ───────────────────────────────────────────

#[cfg(test)]
const fn assert_send<T: Send>() {}

#[cfg(test)]
mod tests {
    use super::*;
    use ikpir_common::backend::frodo::FrodoPirBackend;
    use ikpir_common::backend::simple::{SimpleParams, SimplePirBackend};
    use ikpir_common::pir_params::{frodo_max_plaintext_bits, simple_max_plaintext_bits};
    use ikpir_common::{FrodoConfig, SimpleConfig};
    use risepir_proto::geometry::Geometry;
    use risepir_proto::{Balance, BlockUpdate};
    use risepir_server::RisePirServer;
    use segmented_cuckoo::{Segmented3aryCuckooKVStore, Segmented3aryScheme};
    use std::collections::HashMap;
    use std::sync::Mutex;

    // ── Shared small test geometry, per this crate's spec ───────────────
    const ARITY: u32 = 3;
    const NUM_BUCKETS: u32 = 3 * 1024;
    const BUCKET_SIZE: u32 = 4;
    const FINGERPRINT_BITS: u32 = 32;
    const KEY_TAG_BITS: u32 = 32;
    const BALANCE_BITS: u32 = 96;
    const CHECKSUM_BITS: u32 = 16;
    const LWE_DIM: u32 = 512;

    fn value_codec() -> ValueCodec {
        ValueCodec {
            key_tag_bits: KEY_TAG_BITS,
            balance_bits: BALANCE_BITS,
            checksum_bits: CHECKSUM_BITS,
        }
    }

    /// Deterministic distinct address for index `i` — mirrors
    /// `risepir-server`'s own test helper.
    fn addr(i: u64) -> AddressHash {
        let mut a = [0u8; 32];
        a[..8].copy_from_slice(&i.to_le_bytes());
        a
    }

    /// Small dependency-free deterministic PRNG (xorshift64*), mirroring
    /// `risepir-server`'s own test helper.
    struct Xorshift64(u64);
    impl Xorshift64 {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
    }

    /// Per-backend test setup: derived `plaintext_bits` (each backend's
    /// noise bound differs) and a small `lwe_dim` config for test speed.
    trait BackendTestSetup: IncrementalPirBackend + ResponseRewind {
        fn plaintext_bits(segment_rows: u32) -> u32;
        fn small_config() -> Self::Config;
    }

    impl BackendTestSetup for SimplePirBackend {
        fn plaintext_bits(segment_rows: u32) -> u32 {
            simple_max_plaintext_bits(
                segment_rows,
                BUCKET_SIZE,
                FINGERPRINT_BITS,
                KEY_TAG_BITS + BALANCE_BITS + CHECKSUM_BITS,
                SimpleParams::DEFAULT_SIGMA,
            )
        }
        fn small_config() -> SimpleConfig {
            SimpleConfig::with_lwe_dim(LWE_DIM)
        }
    }

    impl BackendTestSetup for FrodoPirBackend {
        fn plaintext_bits(segment_rows: u32) -> u32 {
            frodo_max_plaintext_bits(segment_rows)
        }
        fn small_config() -> FrodoConfig {
            FrodoConfig::with_lwe_dim(LWE_DIM)
        }
    }

    fn geometry<B: BackendTestSetup>() -> Geometry {
        let segment_rows = NUM_BUCKETS / ARITY;
        let plaintext_bits = B::plaintext_bits(segment_rows);
        Geometry {
            arity: ARITY,
            num_buckets: NUM_BUCKETS,
            bucket_size: BUCKET_SIZE,
            fingerprint_bits: FINGERPRINT_BITS,
            value_bits: KEY_TAG_BITS + BALANCE_BITS + CHECKSUM_BITS,
            plaintext_bits,
        }
    }

    fn empty_store(geom: &Geometry) -> Segmented3aryCuckooKVStore {
        Segmented3aryCuckooKVStore::new(
            geom.num_buckets,
            geom.bucket_size,
            geom.fingerprint_bits,
            geom.value_bits,
            geom.plaintext_bits,
        )
        .unwrap()
    }

    fn server<B: BackendTestSetup>(geom: &Geometry) -> RisePirServer<Segmented3aryScheme, B> {
        RisePirServer::new(empty_store(geom), B::small_config(), value_codec(), 0)
    }

    /// Test-only re-implementation of `finish`'s steps 2-5, with knobs to
    /// selectively skip step 4 (the ordering trap, test 2) or inject
    /// corruption into a decoded value cell (test 6). Mirrors `finish`'s
    /// JOINT fp + `key_tag` per-slot select (ADR-0009) using an ordinary
    /// `&&` rather than the branchless bit-mask trick — side-channel
    /// hardening is `finish`-only and irrelevant to what these negative
    /// controls check.
    fn finish_raw<B: BackendTestSetup>(
        client: &RisePirClient<B>,
        key: &AddressHash,
        ctx: &QueryCtx<B>,
        mut resp: Vec<B::Response>,
        apply_step4: bool,
        corrupt: Option<fn(&mut [u32])>,
    ) -> Lookup {
        let arity = client.params.arity();
        let (fp, _indices) = client.params.candidate_buckets(key);
        let key_tag = client.value_codec.key_tag(key);
        let bucket_size = client.params.bucket_size as usize;
        let cps = client.params.cells_per_slot() as usize;
        let value_size = client.params.value_size_in_bytes();

        let mut acc = vec![0u8; value_size];
        let mut found = false;

        // `j` addresses four parallel per-segment structures; the indexed
        // form keeps them visibly in lockstep.
        #[allow(clippy::needless_range_loop)]
        for j in 0..arity {
            B::rewind_response(
                &client.states[j],
                &ctx.queries[j],
                &mut resp[j],
                client.delta.segment(j),
            );
            let mut cells: Vec<u32> = B::client_decode(&client.states[j], &resp[j]);

            if apply_step4 {
                let row = ctx.rows[j];
                for (offset, delta) in client.delta.row(j, row) {
                    let idx = offset as usize;
                    cells[idx] = (i64::from(cells[idx]) + delta) as u32;
                }
            }

            for s in 0..bucket_size {
                let slot = &mut cells[s * cps..(s + 1) * cps];
                let (decoded_fp, value_bytes) = unpack_slot_cells(&client.params, slot);
                if decoded_fp == fp && client.value_codec.extract_key_tag(&value_bytes) == key_tag {
                    if let Some(f) = corrupt {
                        f(slot);
                    }
                    let (_, value_bytes) = unpack_slot_cells(&client.params, slot);
                    for (a, v) in acc.iter_mut().zip(value_bytes.iter()) {
                        *a = *v;
                    }
                    found = true;
                }
            }
        }

        if !found {
            return Lookup::NotFound;
        }
        client.value_codec.decode(key, &acc)
    }

    // ── 1. created_during_run (parameterised over both backends) ────────

    fn created_during_run_generic<B>()
    where
        B: BackendTestSetup,
        B::Query: Clone,
    {
        let geom = geometry::<B>();
        let mut srv: RisePirServer<Segmented3aryScheme, B> = server::<B>(&geom);

        let genesis: Vec<(AddressHash, Balance)> = (0..50u64)
            .map(|i| (addr(i), 10_000u128 + u128::from(i)))
            .collect();
        srv.apply_block(&BlockUpdate {
            block: 1,
            changes: genesis,
            credits: vec![],
        })
        .unwrap();

        let mut client: RisePirClient<B> = RisePirClient::from_setup(srv.setup(), value_codec());
        assert_eq!(client.pinned_block(), 1);

        const TARGET_BLOCK: u64 = 150;
        let target = addr(10_000_000);
        let target_balance: Balance = 123_456_789_000_000_000_000u128;

        let mut next_id = 1_000u64;
        for block in 2u64..=320 {
            let mut changes: Vec<(AddressHash, Balance)> = (0..8)
                .map(|_| {
                    next_id += 1;
                    (addr(next_id), u128::from(next_id) * 1_000_000_000_000u128)
                })
                .collect();
            if block == TARGET_BLOCK {
                changes.push((target, target_balance));
            }
            let delta = srv.apply_block(&BlockUpdate { block, changes, credits: vec![] }).unwrap();
            client.ingest_delta(&delta).unwrap();
        }
        assert!(
            srv.block() >= 300,
            "server must have advanced >= 300 blocks"
        );
        assert_eq!(
            client.pinned_block(),
            1,
            "hint must remain pinned at genesis; no GC happened"
        );

        let (queries, ctx) = client.build_query(&target);
        let (resp, at_block) = srv.answer(&queries).unwrap();
        let result = client.finish(&target, &ctx, resp, at_block).unwrap();
        assert_eq!(
            result,
            Lookup::Found(target_balance),
            "account created after the pin must resolve to its correct balance"
        );
    }

    #[test]
    fn created_during_run_simple() {
        created_during_run_generic::<SimplePirBackend>();
    }

    #[test]
    fn created_during_run_frodo() {
        created_during_run_generic::<FrodoPirBackend>();
    }

    // ── 2. ordering_trap_is_real ─────────────────────────────────────────

    #[test]
    fn ordering_trap_is_real() {
        let geom = geometry::<SimplePirBackend>();
        let mut srv: RisePirServer<Segmented3aryScheme, SimplePirBackend> =
            server::<SimplePirBackend>(&geom);
        srv.apply_block(&BlockUpdate {
            block: 1,
            changes: vec![(addr(0), 10_000u128)],
            credits: vec![],
        })
        .unwrap();

        let mut client: RisePirClient<SimplePirBackend> =
            RisePirClient::from_setup(srv.setup(), value_codec());

        let target = addr(777);
        let target_balance: Balance = 9_999_000_000_000_000_000u128;
        let delta = srv
            .apply_block(&BlockUpdate {
                block: 2,
                changes: vec![(target, target_balance)],
                credits: vec![],
            })
            .unwrap();
        client.ingest_delta(&delta).unwrap();

        let (queries, ctx) = client.build_query(&target);

        // Negative control: steps 1-3 then scan WITHOUT step 4.
        let (resp1, at_block1) = srv.answer(&queries).unwrap();
        assert_eq!(at_block1, 2);
        let without_step4 = finish_raw(&client, &target, &ctx, resp1, false, None);
        assert_eq!(
            without_step4,
            Lookup::NotFound,
            "THE ORDERING TRAP DID NOT REPRODUCE: scanning before applying ΔD[row] still found \
             the post-pin account. This is a finding, not a passing test — the negative control \
             is not exercising what it claims to; investigate before trusting `finish`'s step \
             ordering."
        );

        // Positive: the real `finish` (which applies step 4, in order) must find it.
        let (resp2, at_block2) = srv.answer(&queries).unwrap();
        assert_eq!(
            at_block2, at_block1,
            "server head must not have moved between the two answers"
        );
        let corrected = client.finish(&target, &ctx, resp2, at_block2).unwrap();
        assert_eq!(corrected, Lookup::Found(target_balance));
    }

    // ── 3. stale_hint_never_patched (parameterised over both backends) ──

    fn stale_hint_never_patched_generic<B>()
    where
        B: BackendTestSetup,
        B::Query: Clone,
    {
        let geom = geometry::<B>();
        let mut srv: RisePirServer<Segmented3aryScheme, B> = server::<B>(&geom);

        let genesis: Vec<(AddressHash, Balance)> = (0..100u64)
            .map(|i| (addr(i), 1_000u128 + u128::from(i)))
            .collect();
        srv.apply_block(&BlockUpdate {
            block: 1,
            changes: genesis.clone(),
            credits: vec![],
        })
        .unwrap();

        let mut client: RisePirClient<B> = RisePirClient::from_setup(srv.setup(), value_codec());

        let mut truth: HashMap<AddressHash, Balance> = genesis.into_iter().collect();

        // 7 pre-existing accounts updated partway through; 7 brand-new
        // accounts created after the pin; 6 pre-existing, untouched
        // accounts. 7 + 7 + 6 = 20.
        let updated: Vec<AddressHash> = (0..7u64).map(addr).collect();
        let created: Vec<AddressHash> = (0..7u64).map(|i| addr(100_000 + i)).collect();
        let untouched: Vec<AddressHash> = (50..56u64).map(addr).collect();

        let mut next_new_id = 200_000u64;
        for block in 2u64..=500 {
            let mut changes: Vec<(AddressHash, Balance)> = Vec::new();

            // A little unrelated churn every block so the delta stream is
            // realistic, not just two isolated events.
            next_new_id += 1;
            let churn_balance = u128::from(next_new_id);
            changes.push((addr(next_new_id), churn_balance));
            truth.insert(addr(next_new_id), churn_balance);

            if block == 100 {
                for (k, a) in updated.iter().enumerate() {
                    let new_balance = 9_000_000u128 + k as u128;
                    changes.push((*a, new_balance));
                    truth.insert(*a, new_balance);
                }
            }
            if block == 250 {
                for (k, a) in created.iter().enumerate() {
                    let new_balance = 5_000_000_000u128 + k as u128;
                    changes.push((*a, new_balance));
                    truth.insert(*a, new_balance);
                }
            }

            let delta = srv.apply_block(&BlockUpdate { block, changes, credits: vec![] }).unwrap();
            client.ingest_delta(&delta).unwrap();
        }
        assert_eq!(client.pinned_block(), 1, "never GC'd");
        assert_eq!(srv.block(), 500);

        let mut queried: Vec<AddressHash> = Vec::new();
        queried.extend(updated.iter().copied());
        queried.extend(created.iter().copied());
        queried.extend(untouched.iter().copied());
        assert_eq!(queried.len(), 20);

        for key in &queried {
            let (queries, ctx) = client.build_query(key);
            let (resp, at_block) = srv.answer(&queries).unwrap();
            let result = client.finish(key, &ctx, resp, at_block).unwrap();
            let expected = *truth.get(key).expect("truth must have every queried key");
            assert_eq!(result, Lookup::Found(expected), "key {key:?} mismatch");
        }
    }

    #[test]
    fn stale_hint_never_patched_simple() {
        stale_hint_never_patched_generic::<SimplePirBackend>();
    }

    #[test]
    fn stale_hint_never_patched_frodo() {
        stale_hint_never_patched_generic::<FrodoPirBackend>();
    }

    // ── 4. gc_then_query_agrees ──────────────────────────────────────────

    #[test]
    fn gc_then_query_agrees() {
        let geom = geometry::<SimplePirBackend>();
        let mut srv: RisePirServer<Segmented3aryScheme, SimplePirBackend> =
            server::<SimplePirBackend>(&geom);

        let genesis: Vec<(AddressHash, Balance)> = (0..30u64)
            .map(|i| (addr(i), 1_000u128 + u128::from(i)))
            .collect();
        srv.apply_block(&BlockUpdate {
            block: 1,
            changes: genesis,
            credits: vec![],
        })
        .unwrap();
        let mut client: RisePirClient<SimplePirBackend> =
            RisePirClient::from_setup(srv.setup(), value_codec());

        let mut rng = Xorshift64(0x1234_5678_9ABC_DEF0);
        for block in 2u64..=80 {
            let changes: Vec<(AddressHash, Balance)> = (0..5)
                .map(|_| {
                    let i = rng.next() % 30;
                    (addr(i), u128::from(rng.next() % (1u64 << 40)))
                })
                .collect();
            let delta = srv.apply_block(&BlockUpdate { block, changes, credits: vec![] }).unwrap();
            client.ingest_delta(&delta).unwrap();
        }

        let cells_before_gc = client.delta_cells();
        assert!(
            cells_before_gc > 0,
            "sanity: there must be something pending to GC"
        );

        let keys: Vec<AddressHash> = (0..30u64).map(addr).collect();
        let mut before: Vec<Lookup> = Vec::new();
        for key in &keys {
            let (queries, ctx) = client.build_query(key);
            let (resp, at_block) = srv.answer(&queries).unwrap();
            before.push(client.finish(key, &ctx, resp, at_block).unwrap());
        }

        let head = srv.block();
        client.collect_garbage(head).unwrap();
        assert_eq!(client.pinned_block(), head);
        assert!(
            client.delta_cells() < cells_before_gc,
            "delta_cells() must drop after collect_garbage"
        );
        assert_eq!(
            client.delta_cells(),
            0,
            "the whole pending delta was folded in; nothing should remain"
        );

        let mut after: Vec<Lookup> = Vec::new();
        for key in &keys {
            let (queries, ctx) = client.build_query(key);
            let (resp, at_block) = srv.answer(&queries).unwrap();
            after.push(client.finish(key, &ctx, resp, at_block).unwrap());
        }

        assert_eq!(
            before, after,
            "queries before/after collect_garbage must agree"
        );
    }

    // ── 5. never_existed_is_not_found ────────────────────────────────────

    #[test]
    fn never_existed_is_not_found() {
        let geom = geometry::<SimplePirBackend>();
        let mut srv: RisePirServer<Segmented3aryScheme, SimplePirBackend> =
            server::<SimplePirBackend>(&geom);
        let genesis: Vec<(AddressHash, Balance)> = (0..500u64)
            .map(|i| (addr(i), 1_000u128 + u128::from(i)))
            .collect();
        srv.apply_block(&BlockUpdate {
            block: 1,
            changes: genesis,
            credits: vec![],
        })
        .unwrap();

        let mut client: RisePirClient<SimplePirBackend> =
            RisePirClient::from_setup(srv.setup(), value_codec());

        let mut rng = Xorshift64(0xFEED_FACE_0BAD_F00D);
        for _ in 0..25 {
            let mut random_addr = [0u8; 32];
            for chunk in random_addr.chunks_mut(8) {
                chunk.copy_from_slice(&rng.next().to_le_bytes());
            }
            let (queries, ctx) = client.build_query(&random_addr);
            let (resp, at_block) = srv.answer(&queries).unwrap();
            let result = client.finish(&random_addr, &ctx, resp, at_block).unwrap();
            assert_eq!(
                result,
                Lookup::NotFound,
                "random address {random_addr:?} must not be found"
            );
        }
    }

    // ── 6. checksum_catches_corruption ───────────────────────────────────

    #[test]
    fn checksum_catches_corruption() {
        let geom = geometry::<SimplePirBackend>();
        let mut srv: RisePirServer<Segmented3aryScheme, SimplePirBackend> =
            server::<SimplePirBackend>(&geom);
        let target = addr(555);
        let target_balance: Balance = 7_000_000_000_000_000_000u128;
        srv.apply_block(&BlockUpdate {
            block: 1,
            changes: vec![(target, target_balance)],
            credits: vec![],
        })
        .unwrap();

        let mut client: RisePirClient<SimplePirBackend> =
            RisePirClient::from_setup(srv.setup(), value_codec());

        let (queries, ctx) = client.build_query(&target);

        // Sanity: the uncorrupted pipeline must find the correct balance first.
        let (resp_check, at_block_check) = srv.answer(&queries).unwrap();
        let sane = client
            .finish(&target, &ctx, resp_check, at_block_check)
            .unwrap();
        assert_eq!(sane, Lookup::Found(target_balance));

        // Corrupt exactly one value-region cell (never the fingerprint, and
        // — now that the value layout is key_tag ‖ balance ‖ checksum,
        // ADR-0009 — never the key_tag either, which sits at the *low* end
        // of the value bits, closest to the fingerprint) of the matching
        // slot, post-decode/post-step-4, pre-scan. The slot's *last* cell
        // is always deep in the checksum region for this geometry
        // (value_bits=144 spans multiple cells at any pb <= 14, and
        // checksum occupies only the top 16 bits, so the boundary cell
        // shared with the fingerprint or key_tag, if any, is never the
        // last one).
        let corrupt_last_cell: fn(&mut [u32]) = |slot| {
            let last = slot.len() - 1;
            slot[last] ^= 1;
        };
        let (resp, _at_block) = srv.answer(&queries).unwrap();
        let corrupted = finish_raw(&client, &target, &ctx, resp, true, Some(corrupt_last_cell));
        assert_eq!(
            corrupted,
            Lookup::DecodeFailed,
            "a corrupted value cell must be caught by the checksum, never returned as a number"
        );
    }

    // ── 7. send_sync ─────────────────────────────────────────────────────

    #[test]
    fn send_sync() {
        assert_send::<Mutex<RisePirClient<SimplePirBackend>>>();
    }

    // ── 8. false_positive_rate_is_negligible ─────────────────────────────

    /// ADR-0009's core empirical claim, at a deliberately adversarial
    /// geometry: even when the store's OWN fingerprint is weakened to just
    /// `WEAK_FP_BITS = 8` (valid for arity 3 / bucket_size 4, whose own
    /// minimum is 4 bits, per `Segmented3aryCuckooKVStore`'s docs) — far
    /// below the deployment default of 32, specifically so that fp-only
    /// collisions on nonexistent keys are *common*, not a rare event this
    /// test could vacuously fail to observe — requiring the value's
    /// 32-bit `key_tag` to ALSO match still drives the combined
    /// false-positive rate down to the `WEAK_FP_BITS + KEY_TAG_BITS` =
    /// 40-bit-effective level (2⁻⁶⁰ at the deployment default of
    /// `fp=32/key_tag=32`): zero false `Found` / `DecodeFailed` results
    /// across 100,000 negative queries.
    ///
    /// # Why this operates below `RisePirClient`, directly on the store
    ///
    /// The false-positive-resistance claim is about the fp + `key_tag`
    /// *scan* itself (`ValueCodec`/`CuckooParams::candidate_buckets`), not
    /// about anything PIR/LWE-specific — so this queries
    /// `CuckooKVStore::as_cells()` directly (unpacking each negative
    /// query's candidate slots exactly as `RisePirClient::finish` would,
    /// minus the PIR round trip) rather than driving 100,000 real queries
    /// through a `RisePirServer`/`RisePirClient` pair, which would be far
    /// too slow for a unit test. [`never_existed_is_not_found`] already
    /// covers a small end-to-end slice for full-pipeline fidelity.
    ///
    /// # The non-vacuity control
    ///
    /// A test that always trivially passes (e.g. because collisions never
    /// actually happen at this sample size) would prove nothing. So this
    /// also counts how many candidate slots matched on the *weak 8-bit fp
    /// alone* (ignoring `key_tag` entirely — i.e. what a fp-only cuckoo
    /// filter would have accepted) and asserts that count is `> 0`:
    /// confirmation that real fp collisions occurred among the 100,000
    /// negative queries, and that `key_tag` is doing genuine work to
    /// reject every one of them, not merely that none happened to arise.
    #[test]
    fn false_positive_rate_is_negligible() {
        const WEAK_FP_BITS: u32 = 8;
        const NUM_QUERIES: u64 = 100_000;
        // Comfortably disjoint from the inserted id range (below).
        const QUERY_ID_BASE: u64 = 50_000_000;

        let codec = ValueCodec {
            key_tag_bits: KEY_TAG_BITS,
            balance_bits: BALANCE_BITS,
            checksum_bits: CHECKSUM_BITS,
        };
        let value_bits = codec.value_bits();
        let segment_rows = NUM_BUCKETS / ARITY;
        let plaintext_bits = simple_max_plaintext_bits(
            segment_rows,
            BUCKET_SIZE,
            WEAK_FP_BITS,
            value_bits,
            SimpleParams::DEFAULT_SIGMA,
        );
        let mut store: Segmented3aryCuckooKVStore =
            Segmented3aryCuckooKVStore::new(NUM_BUCKETS, BUCKET_SIZE, WEAK_FP_BITS, value_bits, plaintext_bits)
                .unwrap();

        // ~60% load: a genuinely full store (so candidate buckets are
        // densely occupied, maximising the chance of an fp collision)
        // without courting `TableFull` flakiness from the weak 8-bit fp
        // increasing cuckoo-kick pressure.
        let total_slots = u64::from(NUM_BUCKETS) * u64::from(BUCKET_SIZE);
        let num_keys = total_slots * 6 / 10;
        for i in 0..num_keys {
            let a = addr(i);
            let balance: Balance = 1_000_000_000_000_000_000u128 + u128::from(i);
            let v = codec.encode(&a, balance).unwrap();
            store.insert(a, &v).unwrap();
        }

        let params = store.params();
        let cells = store.as_cells();
        let bucket_size = params.bucket_size as usize;

        let mut found_count: u64 = 0;
        let mut decode_failed_count: u64 = 0;
        let mut fp_only_collisions: u64 = 0;

        for q in 0..NUM_QUERIES {
            let key = addr(QUERY_ID_BASE + q);
            let (fp, indices) = params.candidate_buckets(&key);
            let key_tag = codec.key_tag(&key);

            // `indices` is a fixed `[u32; 4]`; only the first `arity`
            // entries are meaningful, so the bound is load-bearing.
            #[allow(clippy::needless_range_loop)]
            for j in 0..params.arity() {
                let bucket = indices[j];
                for s in 0..bucket_size {
                    let slot = &cells[params.slot_cell_range(bucket, s as u32)];
                    let (decoded_fp, value_bytes) = unpack_slot_cells(&params, slot);
                    if decoded_fp != fp {
                        continue;
                    }
                    // A fp-only cuckoo filter would have accepted this
                    // slot as a match — the non-vacuity control.
                    fp_only_collisions += 1;
                    if codec.extract_key_tag(&value_bytes) != key_tag {
                        continue; // key_tag rejects it — correctly NotFound.
                    }
                    match codec.decode(&key, &value_bytes) {
                        Lookup::Found(_) => found_count += 1,
                        Lookup::DecodeFailed => decode_failed_count += 1,
                        Lookup::NotFound => {}
                    }
                }
            }
        }

        assert_eq!(
            found_count, 0,
            "a nonexistent address must never resolve Found — the 64-bit-effective (here \
             40-bit, WEAK_FP_BITS + KEY_TAG_BITS) fingerprint must have rejected every \
             fp-colliding slot via key_tag"
        );
        assert_eq!(
            decode_failed_count, 0,
            "a nonexistent address must never resolve DecodeFailed either — decode is only \
             reached once key_tag already agrees, and there is no LWE noise in this scan-level \
             test to corrupt a checksum"
        );
        assert!(
            fp_only_collisions > 0,
            "non-vacuity control failed: with fingerprint_bits={WEAK_FP_BITS}, {NUM_QUERIES} \
             negative queries across arity={}*bucket_size={bucket_size} candidate slots must \
             have produced at least one raw fp-only collision, or this test isn't exercising \
             the key_tag-vs-fp-only distinction it claims to",
            params.arity()
        );
    }
}
