//! [`RisePirServer`] — batched server over the public
//! `segmented_cuckoo` / `ikpir_common` primitives (`docs/plan.md` §3.2,
//! ADR-0001).
//!
//! # Why not `IkpirServer`
//!
//! `ikpir_server::IkpirServer::{insert,update,delete}` each call a
//! private `commit_mutations()` that drains the mutation log, folds it,
//! patches the hint, and bumps the epoch — **per call**. A 300-change
//! block would therefore cost 300 folds, 300 hint patches, and 300
//! epochs, when this design needs exactly one of each per Ethereum block
//! (`docs/plan.md` §3.4: "epoch = block"). There is no batched entry
//! point upstream, and the folding step (`hint_patch::fold_mutations_into_row_deltas`)
//! is crate-private, so this module reimplements it
//! ([`crate::fold::fold_mutations_into_row_deltas`]) over
//! `segmented_cuckoo`'s public types and drives the same per-segment
//! setup/answer/patch plumbing `IkpirServer` uses, minus the
//! per-mutation epoch bump and minus the query-side epoch check (see
//! [`RisePirServer::answer`]). Measured 2.1–2.3× faster
//! (`docs/verification.md` Correction 3) — a deliberate, measured
//! decision, not a shortcut.

use ikpir_common::{HintPatchMode, IncrementalPirBackend, IndexPirBackend};
use segmented_cuckoo::{CuckooError, CuckooKVStore, CuckooParams, IndexScheme, SchemeMeta};

use risepir_proto::{AddressHash, Balance, BlockDelta, BlockUpdate, SegmentRowDeltas, ValueCodec};

use crate::error::ServerError;
use crate::fold::fold_mutations_into_row_deltas;
use crate::verified::{self, Located};

/// Snapshot of a [`RisePirServer`]'s full preprocessing state, e.g. for
/// bootstrapping a fresh client or for tests that need to inspect hints
/// directly.
///
/// Mirrors `ikpir_common::wire::ServerSetupBundle` in shape, with `epoch`
/// renamed to [`Self::block`] — this design has no separate epoch
/// counter; the block number **is** the epoch (`docs/plan.md` §3.4).
pub struct SetupBundle<B: IndexPirBackend> {
    /// Geometry of the underlying SCF KV store.
    pub params: CuckooParams,
    /// One [`IndexPirBackend::ServerParams`] per segment; length equals
    /// `params.arity()`.
    pub backend_params: Vec<B::ServerParams>,
    /// One [`IndexPirBackend::Hint`] per segment; length equals
    /// `params.arity()`.
    pub hints: Vec<B::Hint>,
    /// The block this snapshot was taken at.
    pub block: u64,
}

// Manual `Clone` (rather than `#[derive(Clone)]`) so this bundle is
// `Clone` whenever `B::ServerParams` / `B::Hint` are — which the trait
// already guarantees (`IndexPirBackend::ServerParams: Clone`,
// `IndexPirBackend::Hint: Clone`) — without also demanding `B: Clone`,
// which `#[derive(Clone)]` would incorrectly require here since `B`
// itself never appears as a stored field, only through its associated
// types. `ikpir_common::wire::HintDeltaBundle` takes the same manual
// approach for the same reason.
impl<B: IndexPirBackend> Clone for SetupBundle<B> {
    fn clone(&self) -> Self {
        Self {
            params: self.params,
            backend_params: self.backend_params.clone(),
            hints: self.hints.clone(),
            block: self.block,
        }
    }
}

/// Batched RisePIR server: one epoch and one delta bundle per Ethereum
/// block, generic over the SCF scheme `S` and PIR backend `B` (this
/// project's chosen backend is `SimplePirBackend` — RisePIR-S,
/// `docs/verification.md` Correction 5 — but nothing here hardcodes it).
///
/// # Invariant this type maintains (when every call succeeds)
///
/// `hints[j] == A_j · D_j` for every segment `j`, where `D_j` is
/// segment `j`'s current flat cell array (`store.as_cells()`) and `A_j`
/// is that segment's public LWE matrix. [`RisePirServer::answer`] reads
/// `D_j` directly and is only meaningful to a client whose own hint
/// tracks this same invariant (possibly patched forward via the
/// [`BlockDelta`] stream rather than by holding a live server
/// connection). See [`RisePirServer::apply_block`]'s documentation for
/// the one place this invariant can be knowingly, honestly violated.
pub struct RisePirServer<S: IndexScheme + SchemeMeta, B: IncrementalPirBackend> {
    store: CuckooKVStore<S>,
    params: CuckooParams,
    /// Persisted so [`Self::full_rebuild`] can re-derive hints of the
    /// same dimensions without the caller re-supplying the config. Not
    /// listed in the task brief's struct sketch, but required for
    /// `full_rebuild(&mut self)` to take no arguments — see this crate's
    /// top-level report for why this field had to be added.
    backend_config: B::Config,
    backend_params: Vec<B::ServerParams>,
    material: Vec<B::HintMaterial>,
    hints: Vec<B::Hint>,
    /// Our epoch **is** the block number (`docs/plan.md` §3.4) — there is
    /// no separate counter.
    block: u64,
    value_codec: ValueCodec,
}

impl<S: IndexScheme + SchemeMeta, B: IncrementalPirBackend> RisePirServer<S, B> {
    /// Build a server from a populated `CuckooKVStore` (the state as of
    /// `genesis_block`), a backend config, and the value codec every
    /// balance will be encoded/decoded through.
    ///
    /// Mirrors `IkpirServer::new`: runs `B::server_setup` once per
    /// segment over the store's current cells, then enables the store's
    /// mutation log and drains it once (clearing any mutations the
    /// caller's own genesis population left buffered, so the first
    /// [`Self::apply_block`] starts from an empty log).
    ///
    /// # Panics
    ///
    /// If `value_codec`'s encoded width (`⌈(balance_bits + checksum_bits)
    /// / 8⌉` bytes) does not match `store.value_size_in_bytes()`. This is
    /// a deployment configuration bug — the two must be sized
    /// consistently by whoever builds the geometry — not a condition
    /// arising from any input this crate later accepts, so a loud panic
    /// at construction is preferable to a confusing store-rejected-write
    /// error deep inside the first [`Self::apply_block`] call.
    pub fn new(
        mut store: CuckooKVStore<S>,
        config: B::Config,
        value_codec: ValueCodec,
        genesis_block: u64,
    ) -> Self {
        let expected_value_bytes = (value_codec.value_bits() as usize).div_ceil(8);
        let store_value_bytes = store.value_size_in_bytes();
        assert_eq!(
            expected_value_bytes, store_value_bytes,
            "RisePirServer::new: value_codec encodes to {expected_value_bytes} bytes but the \
             store is configured for {store_value_bytes}-byte values — value_codec's \
             value_bits() (key_tag_bits + balance_bits + checksum_bits) and the \
             store/geometry's value_bits must agree"
        );

        let params = store.params();
        let arity = params.arity();
        let segment_size = params.segment_size();
        let row_width = params.bucket_size * params.cells_per_slot();
        let pb = params.plaintext_bits;
        let seg_cells = segment_size as usize * row_width as usize;

        let mut backend_params = Vec::with_capacity(arity);
        let mut material = Vec::with_capacity(arity);
        let mut hints = Vec::with_capacity(arity);
        {
            let cells = store.as_cells();
            for j in 0..arity {
                let start = j * seg_cells;
                let (sp, mat, h) = B::server_setup(
                    &config,
                    &cells[start..start + seg_cells],
                    segment_size,
                    row_width,
                    pb,
                );
                backend_params.push(sp);
                material.push(mat);
                hints.push(h);
            }
        }

        store.enable_mutation_log();
        let _ = store.drain_mutations();

        Self {
            store,
            params,
            backend_config: config,
            backend_params,
            material,
            hints,
            block: genesis_block,
            value_codec,
        }
    }

    /// Reassemble a server from persisted parts — the restart path
    /// (`docs/deploy.md`): a store rebuilt via `from_cells`, plus the
    /// exact `backend_params` and `hints` a previous run's
    /// [`Self::setup`] snapshot carried. **No `server_setup` runs**: `A`
    /// stays bit-identical to the persisted deployment, so clients
    /// bootstrapped before the restart keep decoding correctly, and
    /// startup cost is I/O, not a full preprocessing pass.
    ///
    /// `material` is re-expanded deterministically from each segment's
    /// params ([`IndexPirBackend::expand_hint_material`] — determinism is
    /// part of that trait's documented contract).
    ///
    /// # Panics
    ///
    /// Same value-width consistency panic as [`Self::new`], plus a panic
    /// if `backend_params` / `hints` lengths do not equal the store's
    /// arity — both are corrupt-state-file / configuration failures at
    /// construction time, not live-input conditions.
    pub fn from_parts(
        mut store: CuckooKVStore<S>,
        config: B::Config,
        value_codec: ValueCodec,
        backend_params: Vec<B::ServerParams>,
        hints: Vec<B::Hint>,
        block: u64,
    ) -> Self {
        let expected_value_bytes = (value_codec.value_bits() as usize).div_ceil(8);
        let store_value_bytes = store.value_size_in_bytes();
        assert_eq!(
            expected_value_bytes, store_value_bytes,
            "RisePirServer::from_parts: value_codec encodes to {expected_value_bytes} bytes but \
             the store is configured for {store_value_bytes}-byte values"
        );
        let params = store.params();
        let arity = params.arity();
        assert_eq!(
            backend_params.len(),
            arity,
            "RisePirServer::from_parts: {} backend_params for arity {arity}",
            backend_params.len()
        );
        assert_eq!(
            hints.len(),
            arity,
            "RisePirServer::from_parts: {} hints for arity {arity}",
            hints.len()
        );

        let material: Vec<B::HintMaterial> =
            backend_params.iter().map(B::expand_hint_material).collect();

        store.enable_mutation_log();
        let _ = store.drain_mutations();

        Self {
            store,
            params,
            backend_config: config,
            backend_params,
            material,
            hints,
            block,
            value_codec,
        }
    }

    /// A copy of the store's flat cell array — together with
    /// [`Self::setup`], [`Self::num_items`], and the value codec, exactly
    /// what a state file must persist for [`Self::from_parts`] to
    /// reassemble this server.
    ///
    /// At the complete mainnet set this array is ~23.6 GB at the deployed
    /// `(arity 2, bucket_size 4)` geometry (ADR-0034; ~35 GB at the previous
    /// `(arity 3, bucket_size 4)` geometry, which is what the live
    /// deployment still actually runs until it is re-bootstrapped), so a
    /// caller that only *reads* the cells (state serialization, above all)
    /// must use [`Self::cells`] instead — see that method.
    pub fn snapshot_cells(&self) -> Vec<u32> {
        self.store.snapshot_cells()
    }

    /// The store's flat cell array, borrowed — the read-only twin of
    /// [`Self::snapshot_cells`], with identical contents.
    ///
    /// # Why this exists
    ///
    /// The cell array is the whole PIR database: `num_buckets *
    /// bucket_size * cells_per_slot` `u32`s, which at the complete
    /// mainnet set (200.5 M accounts) is **23.62 GB** at the deployed
    /// `(arity 2, bucket_size 4)` geometry — 67,108,864 buckets (ADR-0034);
    /// it was **35.4 GB** at 100,663,296 buckets under the previous
    /// `(arity 3, bucket_size 4)` geometry, which is what the live
    /// deployment still actually runs until it is re-bootstrapped.
    /// `snapshot_cells` allocates a second one, so any caller that copies
    /// merely to read — as state serialization did — doubles the
    /// process's peak RSS (to ~47 GB at the current geometry, ~73 GB at
    /// the previous one) and decides what machine the deployment needs.
    /// Streaming from this borrow keeps a state save allocation-free.
    pub fn cells(&self) -> &[u32] {
        self.store.as_cells()
    }

    /// Number of `(address, balance)` entries currently stored.
    pub fn num_items(&self) -> u64 {
        self.store.num_items()
    }

    /// Discards buffered mutations and returns `err` — the single exit
    /// path every [`Self::apply_block`] failure funnels through. See
    /// [`Self::apply_block`]'s documentation for exactly what this does
    /// and does not guarantee about the resulting store/hint state.
    fn reject_block(&mut self, err: ServerError) -> ServerError {
        let _ = self.store.drain_mutations();
        err
    }

    /// Apply one `(address, absolute balance)` mutation through the
    /// verified-scan dispatch (ADR-0017; see [`Self::apply_block`]'s
    /// "per-change handling"). Does **not** drain the mutation log on
    /// failure — the caller funnels every error through
    /// [`Self::reject_block`].
    fn apply_change(&mut self, addr: &AddressHash, balance: Balance) -> Result<(), ServerError> {
        let located = verified::locate(&self.store, &self.value_codec, addr);

        if balance == 0 {
            // Delete-on-zero (ADR-0015): the database holds only *nonzero*
            // balances — a lookup that misses answers `0x0` by absence —
            // so zero is a **deletion**, never a stored zero.
            return match located {
                Located::Own(_) => match self.store.delete(addr) {
                    Ok(()) => Ok(()),
                    // `locate` just proved the first fp-match is ours, so
                    // `delete` cannot miss; surfaced defensively.
                    Err(e) => Err(ServerError::Store(format!(
                        "delete failed after verified locate said Own: {e:?}"
                    ))),
                },
                // The load-bearing arm: absent means NO-OP even when
                // foreign fp-matches exist — upstream's fp-only delete
                // would have destroyed one of them.
                Located::Absent { .. } => Ok(()),
                Located::Shadowed => Err(ServerError::FingerprintAmbiguity { shadowed: true }),
                Located::DuplicateTag => Err(ServerError::FingerprintAmbiguity { shadowed: false }),
                Located::Corrupt => Err(ServerError::CorruptStoredValue),
            };
        }

        let encoded = self
            .value_codec
            .encode(addr, balance)
            .map_err(ServerError::Encode)?;

        match located {
            Located::Own(_) => match self.store.update(addr, &encoded) {
                Ok(()) => Ok(()),
                Err(e) => Err(ServerError::Store(format!(
                    "update failed after verified locate said Own: {e:?}"
                ))),
            },
            Located::Absent { .. } => match self.store.insert(addr, &encoded) {
                Ok(()) => Ok(()),
                Err(CuckooError::TableFull) => Err(ServerError::TableFull),
                Err(CuckooError::InvalidParams(msg)) => Err(ServerError::Store(msg)),
                Err(CuckooError::NotFound) => Err(ServerError::Store(
                    "CuckooKVStore::insert returned NotFound, which its documented \
                     contract says cannot happen"
                        .to_string(),
                )),
            },
            Located::Shadowed => Err(ServerError::FingerprintAmbiguity { shadowed: true }),
            Located::DuplicateTag => Err(ServerError::FingerprintAmbiguity { shadowed: false }),
            Located::Corrupt => Err(ServerError::CorruptStoredValue),
        }
    }

    /// Verified read of the balance currently stored for `addr` —
    /// `Ok(None)` if absent, and an error (never a guess) on fingerprint
    /// ambiguity or a checksum-corrupt slot. This is the read the
    /// reconciliation loop compares against a reference RPC
    /// (`docs/sync.md`), and the one place the feed's withdrawal handling
    /// resolves prior values through (ADR-0016: the store is the
    /// authority; `CuckooKVStore::get`'s fp-only first-match is exactly
    /// what this method exists to avoid).
    pub fn balance_of(&self, addr: &AddressHash) -> Result<Option<Balance>, ServerError> {
        match verified::get(&self.store, &self.value_codec, addr) {
            Ok(v) => Ok(v),
            Err(Located::Corrupt) => Err(ServerError::CorruptStoredValue),
            Err(_) => Err(ServerError::FingerprintAmbiguity { shadowed: false }),
        }
    }

    /// Applies one block's worth of balance changes as **one** epoch: one
    /// drain of the store's mutation log, one fold into sparse
    /// per-segment row deltas, and one
    /// [`IncrementalPirBackend::server_patch_hint`] call per touched
    /// segment — regardless of how many changes the block carries. This
    /// is the batching `IkpirServer` cannot do; see the module docs.
    ///
    /// # Per-change handling — every write is *verified* first (ADR-0017)
    ///
    /// The store's own key-addressed ops match on the 32-bit fingerprint
    /// alone, first match in probe order — which is how a `(addr, 0)` for
    /// an *absent* account (routinely emitted by a live feed for accounts
    /// a block touches without their balance leaving zero) could destroy
    /// a fingerprint-colliding *foreign* account's entry. So each change
    /// first runs `crate::verified::locate` — the server-side twin of
    /// the client's joint fp + `key_tag` mask — and only then acts:
    ///
    /// For each `(address_hash, balance)` in `update.changes`, in order:
    /// 0. **`balance == 0` ⇒ delete** (`docs/plan.md` ADR-0015): only if
    ///    `locate` proves the key's own entry is the first fp-match
    ///    (`Own`) is [`CuckooKVStore::delete`] called; `Absent` — even
    ///    with foreign fp-matches present, exactly the case upstream
    ///    would have mis-deleted — is a **no-op** (an absent account
    ///    already answers `0x0`).
    /// 1. Otherwise [`risepir_proto::ValueCodec::encode`] — hard-fails
    ///    rather than truncating an overflowing balance (ADR-0009).
    /// 2. `Own` ⇒ [`CuckooKVStore::update`] (provably hits our slot);
    ///    `Absent` ⇒ [`CuckooKVStore::insert`] (writes only empty slots,
    ///    cannot corrupt anyone; kick-budget exhaustion maps to
    ///    [`ServerError::TableFull`]).
    /// 3. `Shadowed` / `DuplicateTag` ⇒
    ///    [`ServerError::FingerprintAmbiguity`] and `Corrupt` ⇒
    ///    [`ServerError::CorruptStoredValue`] — the block is rejected
    ///    loudly; there is no safe key-addressed write in those states.
    ///
    /// Duplicate addresses within one block are allowed, matching
    /// [`BlockUpdate`]'s own documented contract: each occurrence is
    /// applied in order, so the **last** occurrence's balance is the one
    /// left in the store (an `insert` followed by one or more `update`s
    /// to the same brand-new address ends up with exactly one occupied
    /// slot, not one per occurrence).
    ///
    /// # Withdrawal credits
    ///
    /// After every entry of `update.changes`, each `(address_hash,
    /// amount)` in `update.credits` is applied as `stored (or 0 if
    /// absent) + amount` via the same verified read/write path
    /// ([`BlockUpdate::credits`]'s docs explain why credits are relative
    /// while changes are absolute). Credits to the same address
    /// accumulate in order; `checked_add` overflow rejects the block
    /// (never wraps).
    ///
    /// # What "one epoch per block" does *not* mean: the atomicity caveat
    ///
    /// **This is not a transaction.** If a change partway through
    /// `update.changes` fails, every change *before* it has already been
    /// committed to the store's flat cell array — `update` and a
    /// successful `insert` are not undoable from here (there is no
    /// store-level "undo" for a already-committed write; the only
    /// rollback that exists is the kick-chain rollback `insert` already
    /// performs internally on its *own* `TableFull`, before it ever
    /// returns to this method). On any failure, this method:
    ///
    /// - drains the mutation log and **discards** what it drained, so
    ///   that whatever succeeded before the failure is never silently
    ///   folded into a *later*, unrelated block's delta (misattributing
    ///   it to the wrong block number would itself be a
    ///   silent-wrong-answer bug);
    /// - does **not** patch any hint with that discarded prefix;
    /// - does **not** advance [`Self::block`];
    /// - returns `Err` without constructing a [`BlockDelta`].
    ///
    /// The honest consequence: after an `Err` that occurred on anything
    /// but the first change in the block, the store's cells may already
    /// reflect a prefix of `update.changes` that the hints do not — the
    /// `hints[j] == A_j · D_j` invariant [`Self::answer`] depends on for
    /// a client's decode to be meaningful is now violated for exactly the
    /// rows that prefix touched, and **stays violated** until either
    /// [`Self::full_rebuild`] resyncs every hint from the current cells
    /// (at the cost of a full resync for every client — there is no
    /// incremental delta for this repair, by construction; see
    /// `full_rebuild`'s own docs) or the server is reconstructed fresh
    /// from authoritative `(address, balance)` state (the recovery
    /// `IkpirServer::insert`'s own `TableFull` documentation prescribes,
    /// since the cuckoo store never retains keys to self-recover from).
    /// If **no** change in the block reached the store before the
    /// failure — [`ServerError::NonMonotonicBlock`], checked first,
    /// before any store mutation; or a `BalanceOverflow` on the very
    /// first change — no repair is needed; the store and hints are
    /// simply untouched, exactly as if `apply_block` had never been
    /// called.
    ///
    /// # Errors
    ///
    /// See [`ServerError`]. [`ServerError::NonMonotonicBlock`] if
    /// `update.block` is not strictly greater than [`Self::block`]
    /// (checked before touching the store, so always side-effect-free).
    pub fn apply_block(&mut self, update: &BlockUpdate) -> Result<BlockDelta, ServerError> {
        if update.block <= self.block {
            return Err(ServerError::NonMonotonicBlock {
                current: self.block,
                attempted: update.block,
            });
        }

        for (addr, balance) in &update.changes {
            if let Err(e) = self.apply_change(addr, *balance) {
                return Err(self.reject_block(e));
            }
        }

        // Withdrawal credits (EIP-4895): relative amounts, applied after
        // every absolute change — see the method docs and
        // `BlockUpdate::credits`. Each resolves against the store's own
        // verified read (the authoritative prior value, ADR-0016), so a
        // fingerprint-colliding foreign entry can neither be misread as
        // the prior nor be overwritten by the credited write.
        for (addr, amount) in &update.credits {
            let prior = match verified::get(&self.store, &self.value_codec, addr) {
                Ok(Some(b)) => b,
                Ok(None) => 0,
                Err(Located::Corrupt) => {
                    return Err(self.reject_block(ServerError::CorruptStoredValue))
                }
                Err(_) => {
                    return Err(
                        self.reject_block(ServerError::FingerprintAmbiguity { shadowed: false })
                    );
                }
            };
            let Some(credited) = prior.checked_add(*amount) else {
                return Err(self.reject_block(ServerError::Encode(
                    risepir_proto::ValueError::BalanceOverflow,
                )));
            };
            if let Err(e) = self.apply_change(addr, credited) {
                return Err(self.reject_block(e));
            }
        }

        // Whole block succeeded: ONE drain, ONE fold, ONE patch per
        // touched segment — the batching `IkpirServer` cannot do.
        let muts = self.store.drain_mutations();
        let row_deltas: Vec<SegmentRowDeltas> = fold_mutations_into_row_deltas(&muts, &self.params);
        for (j, deltas) in row_deltas.iter().enumerate() {
            if !deltas.is_empty() {
                // EntryLevel: Θ(n) per touched cell, independent of the
                // database size — the realization that keeps per-block
                // patch cost from growing with the store
                // (`docs/verification.md` Correction 4). RowLevel would
                // still be *correct* (both realizations produce
                // bit-identical hints) but pays Θ(n·ω) per touched row.
                B::server_patch_hint(
                    &self.backend_params[j],
                    &self.material[j],
                    &mut self.hints[j],
                    deltas,
                    HintPatchMode::EntryLevel,
                );
            }
        }

        self.block = update.block;
        Ok(BlockDelta {
            block: self.block,
            per_segment: row_deltas,
        })
    }

    /// Answer a per-segment PIR query bundle at the server's current
    /// head.
    ///
    /// # Purpose
    ///
    /// Server hot path: runs `B::server_answer` once per segment over
    /// `self.store.as_cells()` and returns the per-segment responses
    /// together with [`Self::block`] — the response names the epoch it
    /// was computed at (`docs/plan.md` §3.4), rather than the caller
    /// having to have already agreed on one.
    ///
    /// # Deliberately does not check a client epoch
    ///
    /// Unlike `IkpirServer::answer` (which hard-rejects
    /// `q.epoch != self.epoch` with `StaleEpoch`), this method takes no
    /// epoch/block argument from the caller at all — a PIR query depends
    /// only on the deployment's geometry and LWE dimension, never on the
    /// database contents, so answering at the server's head is always
    /// sound no matter how old the query's *construction* is. The client
    /// is responsible for rewinding the response against the coalesced
    /// delta stream back to whatever block it actually holds a hint for
    /// (`docs/plan.md` §3.3) — that mechanism, not a query-time epoch
    /// check, is what keeps this sound.
    ///
    /// # Errors
    ///
    /// [`ServerError::MalformedQuery`] if `queries.len()` does not equal
    /// `params.arity()`. Checked before any segment is touched.
    ///
    /// # Complexity
    ///
    /// `O(arity × n_rows × row_width × lwe_dim)`.
    pub fn answer(&self, queries: &[B::Query]) -> Result<(Vec<B::Response>, u64), ServerError> {
        let arity = self.params.arity();
        if queries.len() != arity {
            return Err(ServerError::MalformedQuery {
                expected: arity,
                got: queries.len(),
            });
        }

        let segment_size = self.params.segment_size();
        let row_width = self.params.bucket_size * self.params.cells_per_slot();
        let seg_cells = segment_size as usize * row_width as usize;
        let cells = self.store.as_cells();

        let mut responses = Vec::with_capacity(arity);
        for (j, query) in queries.iter().enumerate() {
            let start = j * seg_cells;
            let r = B::server_answer(
                &self.backend_params[j],
                &cells[start..start + seg_cells],
                segment_size,
                row_width,
                query,
            );
            responses.push(r);
        }
        Ok((responses, self.block))
    }

    /// Snapshot the full preprocessing state — clones of `params`,
    /// `backend_params`, `hints`, and the current [`Self::block`].
    ///
    /// # Complexity
    ///
    /// `O(arity)` shallow-to-deep clones of `Vec<B::ServerParams>` /
    /// `Vec<B::Hint>` (deep exactly as much as the backend types'
    /// `Clone` impls are).
    pub fn setup(&self) -> SetupBundle<B> {
        SetupBundle {
            params: self.params,
            backend_params: self.backend_params.clone(),
            hints: self.hints.clone(),
            block: self.block,
        }
    }

    /// Recompute every per-segment hint from scratch over the store's
    /// *current* cells, using the config captured at construction time.
    ///
    /// # Purpose
    ///
    /// The measured §1 denominator (full rebuild time vs. per-block patch
    /// time, `docs/plan.md` §2) — but also the repair for a
    /// [`Self::apply_block`] failure that left `hints` stale relative to
    /// `store` for some rows (see that method's atomicity-caveat
    /// section): recomputing from the current cells restores the
    /// `hints[j] == A_j · D_j` invariant unconditionally, without needing
    /// to know which rows (if any) had drifted.
    ///
    /// # What this does not do
    ///
    /// Does **not** change [`Self::block`] — a full rebuild resyncs PIR
    /// state to whatever the cells currently contain, which does not
    /// necessarily correspond to any single well-defined block number
    /// after a partial [`Self::apply_block`] failure (see that method's
    /// docs). It also does not grow the store's geometry: recovering from
    /// `TableFull` by growing `num_buckets` requires a *new*
    /// `RisePirServer` built over a larger store from authoritative
    /// `(address, balance)` state (the cuckoo store never retains keys,
    /// so this crate cannot do that rebuild from its own internal state
    /// alone) — `full_rebuild` only ever re-derives hints for the
    /// **existing** geometry.
    ///
    /// # Do not wire this into a *serving* process without invalidation
    ///
    /// `risepir-http`'s `NodeState` snapshots `backend_params` once at
    /// construction (its `/answer` length checks depend on them) and
    /// caches one encoded `/setup` whose freshness check is
    /// block-distance only — and a rebuild changes hints *without*
    /// moving the block. Nothing invalidates either today, which is
    /// sound only because nothing in a serving process calls this
    /// (`NodeState::with_server` is read-only). If a future recovery
    /// path runs `full_rebuild` inside a live server, it must also
    /// re-derive `NodeState`'s caches, or `/answer` decodes with stale
    /// reshape geometry and `/setup` serves pre-rebuild bytes — garbage
    /// decodes, which complete mode can surface as a wrong `0x0`.
    /// (Lineage epochs — ADR-0033 — deliberately do *not* change across
    /// a rebuild: seeds persist, and same-lineage deltas remain valid.)
    ///
    /// # Memory
    ///
    /// Allocates one new `ServerParams`/`HintMaterial`/`Hint` set per
    /// segment — accumulated in local vectors — while the *previous* set
    /// stays live in `self.backend_params` / `self.material` /
    /// `self.hints` until every segment has been rebuilt and the three
    /// fields are overwritten in one shot at the end. So peak usage is
    /// roughly **two** hint+material sets at once — hundreds of MB at
    /// deployment scale — never the ~35 GB cell array itself:
    /// `self.store.as_cells()` is borrowed below, exactly as [`Self::new`]
    /// and [`Self::answer`] already do, never copied. Building the new
    /// set before swapping is deliberate, not an oversight: it is what
    /// keeps the old, still-answerable state intact for every segment
    /// right up until this method has proven it can replace all of them,
    /// rather than leaving `self` holding a half-old-half-new `hints`
    /// array if something below were to panic partway through arity.
    ///
    /// # Complexity
    ///
    /// Same as [`Self::new`]: `O(arity × n_rows × lwe_dim × row_width)`.
    pub fn full_rebuild(&mut self) {
        let arity = self.params.arity();
        let segment_size = self.params.segment_size();
        let row_width = self.params.bucket_size * self.params.cells_per_slot();
        let pb = self.params.plaintext_bits;
        let seg_cells = segment_size as usize * row_width as usize;
        // Borrowed, not copied: `self.store` is only *read* below, and the
        // writes further down target the disjoint fields
        // `self.backend_params` / `self.material` / `self.hints` — the
        // borrow checker already permits disjoint field borrows of the
        // same `self`, so there was never a real conflict here to
        // snapshot around. A copy is not an option at this crate's
        // deployment scale either way: `self.store.as_cells()` is the
        // whole PIR database — 23.62 GB at the complete mainnet set's
        // deployed `(arity 2, bucket_size 4)` geometry, ADR-0034 (was
        // 35.4 GB at the previous `(arity 3, bucket_size 4)` geometry —
        // see [`Self::cells`]) — so a `.to_vec()` here would hold the
        // original and the copy live simultaneously, taking peak RSS from
        // ~24.7 GB to ~47 GB today (was ~38 GB to ~73 GB pre-ADR-0034) and
        // turning this method's own documented repair path for a failed
        // [`Self::apply_block`] into the very OOM it exists to recover
        // from.
        let cells: &[u32] = self.store.as_cells();

        let mut backend_params = Vec::with_capacity(arity);
        let mut material = Vec::with_capacity(arity);
        let mut hints = Vec::with_capacity(arity);
        for j in 0..arity {
            let start = j * seg_cells;
            let (sp, mat, h) = B::server_setup(
                &self.backend_config,
                &cells[start..start + seg_cells],
                segment_size,
                row_width,
                pb,
            );
            backend_params.push(sp);
            material.push(mat);
            hints.push(h);
        }
        self.backend_params = backend_params;
        self.material = material;
        self.hints = hints;
        let _ = self.store.drain_mutations();
    }

    /// The server's current block. Our epoch **is** the block number
    /// (`docs/plan.md` §3.4) — strictly monotone across
    /// [`Self::apply_block`] calls that return `Ok`.
    pub const fn block(&self) -> u64 {
        self.block
    }

    /// SCF geometry parameters of the wrapped store.
    pub const fn params(&self) -> CuckooParams {
        self.params
    }
}

// ─── Send + Sync static assertion ───────────────────────────────────────────
//
// `docs/plan.md` ADR-0010: the concrete backend/scheme types are
// auto-`Send + Sync` (every field of `RisePirServer` is: the store,
// `CuckooParams`, `B::Config`/`ServerParams`/`HintMaterial`/`Hint` for
// `SimplePirBackend` are all plain owned data with no interior
// mutability or non-`Send`/`Sync` primitives), so `RwLock<RisePirServer>`
// gives concurrent readers on the hot `answer` path. This is checked at
// compile time regardless of whether `cfg(test)` code runs, but is also
// exercised as an explicit `#[test]` below per this crate's spec so it
// shows up by name in `cargo test` output.
#[cfg(test)]
const fn assert_send<T: Send>() {}
#[cfg(test)]
const fn assert_sync<T: Sync>() {}

#[cfg(test)]
mod tests {
    use super::*;
    use ikpir_common::backend::simple::{SimpleParams, SimpleQuery};
    use ikpir_common::pir_params::simple_max_plaintext_bits;
    use ikpir_common::{SimpleConfig, SimplePirBackend};
    use risepir_proto::geometry::Geometry;
    use risepir_proto::{AddressHash, Balance, ValueError};
    use segmented_cuckoo::{Segmented2aryCuckooKVStore, Segmented2aryScheme};

    // ── Shared small test geometry (per this crate's spec: small enough
    // that the whole suite is fast) ──────────────────────────────────────
    const ARITY: u32 = 2;
    const NUM_BUCKETS: u32 = 2 * 1024;
    const BUCKET_SIZE: u32 = 4;
    const FINGERPRINT_BITS: u32 = 32;
    const KEY_TAG_BITS: u32 = 32;
    const BALANCE_BITS: u32 = 96;
    const CHECKSUM_BITS: u32 = 16;
    const LWE_DIM: u32 = 512;

    /// `plaintext_bits` is *derived* from `risepir_proto`/`ikpir_common`'s
    /// own noise-bound analysis, never hardcoded — matching this crate's
    /// own rule and `risepir-proto`'s established pattern of pinning a
    /// `Geometry` via a struct literal with a computed `plaintext_bits`
    /// (see `risepir_proto::geometry`'s own pinning tests).
    ///
    /// `simple_max_plaintext_bits` does not take `lwe_dim` as an
    /// argument at all (SimplePIR's noise bound depends on `sigma` and
    /// the reshaped row count, not the LWE dimension — see
    /// `ikpir_common::pir_params`'s module docs) so this is exactly as
    /// valid for our `lwe_dim = 512` test config as it would be for the
    /// default 1275.
    fn geometry() -> Geometry {
        let value_bits = KEY_TAG_BITS + BALANCE_BITS + CHECKSUM_BITS;
        let segment_rows = NUM_BUCKETS / ARITY;
        let plaintext_bits = simple_max_plaintext_bits(
            ARITY,
            segment_rows,
            BUCKET_SIZE,
            FINGERPRINT_BITS,
            value_bits,
            SimpleParams::DEFAULT_SIGMA,
        );
        Geometry {
            arity: ARITY,
            num_buckets: NUM_BUCKETS,
            bucket_size: BUCKET_SIZE,
            fingerprint_bits: FINGERPRINT_BITS,
            value_bits,
            plaintext_bits,
        }
    }

    fn value_codec() -> ValueCodec {
        ValueCodec {
            key_tag_bits: KEY_TAG_BITS,
            balance_bits: BALANCE_BITS,
            checksum_bits: CHECKSUM_BITS,
        }
    }

    fn config() -> SimpleConfig {
        SimpleConfig {
            lwe_dim: LWE_DIM,
            ..Default::default()
        }
    }

    fn empty_store(geom: &Geometry) -> Segmented2aryCuckooKVStore {
        Segmented2aryCuckooKVStore::new(
            geom.num_buckets,
            geom.bucket_size,
            geom.fingerprint_bits,
            geom.value_bits,
            geom.plaintext_bits,
        )
        .unwrap()
    }

    fn server(geom: &Geometry) -> RisePirServer<Segmented2aryScheme, SimplePirBackend> {
        RisePirServer::new(empty_store(geom), config(), value_codec(), 0)
    }

    /// Deterministic distinct address for index `i` (LE bytes of `i` in
    /// the low 8 bytes, zero-padded) — the exact bytes don't matter, only
    /// that distinct `i` give distinct 32-byte keys.
    fn addr(i: u64) -> AddressHash {
        let mut a = [0u8; 32];
        a[..8].copy_from_slice(&i.to_le_bytes());
        a
    }

    /// Small dependency-free deterministic PRNG (xorshift64*) so tests
    /// don't need a `rand` dev-dependency just to generate varied test
    /// balances — mirrors the style `risepir-proto`'s own fuzz tests use.
    struct Xorshift64(u64);
    impl Xorshift64 {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
    }

    // ── 1. epoch_is_block ───────────────────────────────────────────────

    /// Applying 3 blocks of 100 changes each must produce exactly 3
    /// `BlockDelta`s, each with `.block` matching what was passed, and
    /// each non-empty (100 fresh addresses per block guarantees every
    /// block actually touches cells).
    #[test]
    fn epoch_is_block() {
        let geom = geometry();
        let mut s = server(&geom);
        let mut deltas = Vec::new();

        for block in 1u64..=3 {
            let changes: Vec<(AddressHash, Balance)> = (0..100u64)
                .map(|i| {
                    (
                        addr(block * 1_000 + i),
                        1_000_000_000_000_000_000u128 + u128::from(block * 1_000 + i),
                    )
                })
                .collect();
            let delta = s
                .apply_block(&BlockUpdate {
                    block,
                    changes,
                    credits: vec![],
                })
                .unwrap();
            assert_eq!(delta.block, block);
            assert!(
                delta.per_segment.iter().any(|seg| !seg.is_empty()),
                "block {block}'s delta must be non-empty"
            );
            deltas.push(delta);
        }

        assert_eq!(
            deltas.len(),
            3,
            "exactly 3 BlockDeltas for 3 apply_block calls"
        );
        assert_eq!(s.block(), 3);
    }

    // ── 2. batched_equals_per_mutation ──────────────────────────────────

    /// The decisive equivalence: batching must change cost, not
    /// semantics.
    ///
    /// Two claims, both load-bearing, checked separately because they
    /// need different fixtures to be *meaningful* (see the "why not a
    /// direct hint comparison" note below):
    ///
    /// 1. **Delta content is batching-invariant**, checked against a real
    ///    `ikpir_server::IkpirServer` — an external, independently
    ///    implemented oracle for this crate's [`crate::fold`]
    ///    reimplementation. The same N mutations, folded either one at a
    ///    time (as `IkpirServer::update` does via its own private
    ///    `fold_mutations_into_row_deltas`) or in a single batch (as
    ///    [`RisePirServer::apply_block`] does via this crate's
    ///    reimplementation of the same algorithm), must produce the
    ///    identical sparse `(row, offset, delta)` transcript once
    ///    coalesced with [`BlockDelta::coalesce`].
    /// 2. **Hint-patch linearity**: patching a hint once with the
    ///    *summed* deltas must produce the bit-identical hint to patching
    ///    it N times with the individual deltas, given the same starting
    ///    hint and the same public matrix `A`.
    ///
    /// # Why this test does not compare `IkpirServer`'s hint bytes
    /// directly against `RisePirServer`'s
    ///
    /// `IndexPirBackend::server_setup` samples a fresh random seed for
    /// `A` on every call (`SimplePirBackend::server_setup` calls
    /// `rand::rng().fill_bytes(..)` with no way to inject a fixed seed
    /// through the public API, and there is no public constructor for
    /// either `IkpirServer` or `RisePirServer` that accepts a
    /// pre-built `(ServerParams, Hint)` pair instead of calling
    /// `server_setup` itself). So an independently-constructed
    /// `IkpirServer` and `RisePirServer` — even from byte-identical
    /// starting stores and configs — end up with *different* `A`
    /// matrices and therefore *different* hint bytes, by construction,
    /// regardless of whether either implementation is correct. Directly
    /// asserting `ikpir.setup().hints[j].data == risepir.setup().hints[j].data`
    /// would not test batching semantics at all — it would just test
    /// whether two independent random samples happened to collide, which
    /// they will not. **This is a real, reported constraint of the
    /// upstream public API, not a shortcut taken in this test:** see this
    /// crate's delivery report.
    ///
    /// So claim 2 is instead checked *within* `RisePirServer`'s own
    /// randomness: replay the same N mutations one at a time against a
    /// clone of its own genesis store, patching a clone of its own
    /// genesis hint using its own seed (deterministically re-expanded via
    /// `IndexPirBackend::expand_hint_material`, whose determinism is part
    /// of the trait's documented contract), and compare against
    /// `RisePirServer`'s actual post-`apply_block` hint. That isolates
    /// "batched patch == sequential patch" from "did `server_setup`'s RNG
    /// happen to agree" — the latter is not a claim this design makes or
    /// needs.
    #[test]
    fn batched_equals_per_mutation() {
        let geom = geometry();
        let cfg = config();
        let codec = value_codec();

        // Common genesis: every subsequent change below is an `update`
        // (never an `insert`), so no cuckoo kick is ever triggered —
        // `CuckooKVStore::update` consults no RNG at all — which is what
        // lets every store clone built from this same genesis evolve
        // identically no matter which high-level type drives it.
        const N: u64 = 100;
        let mut genesis_store = empty_store(&geom);
        for i in 0..N {
            let v = codec
                .encode(&addr(i), 1_000_000_000_000_000_000u128 + u128::from(i))
                .unwrap();
            genesis_store.insert(addr(i), &v).unwrap();
        }
        let cells0 = genesis_store.snapshot_cells();
        let params0 = genesis_store.params();
        let n0 = genesis_store.num_items();

        let changes: Vec<(AddressHash, Balance)> = (0..N)
            .map(|i| (addr(i), 9_000_000_000_000_000_000u128 + u128::from(i) * 7))
            .collect();

        // ── Claim 1: delta content, against a real IkpirServer oracle. ──
        let ikpir_store =
            Segmented2aryCuckooKVStore::from_cells(cells0.clone(), params0, n0).unwrap();
        let mut ikpir: ikpir_server::IkpirServer<Segmented2aryScheme, SimplePirBackend> =
            ikpir_server::IkpirServer::new(ikpir_store, cfg.clone());

        let mut per_mutation_deltas = Vec::with_capacity(changes.len());
        for (a, bal) in &changes {
            let v = codec.encode(a, *bal).unwrap();
            let bundle = ikpir
                .update(a, &v)
                .expect("every address was pre-inserted into genesis");
            per_mutation_deltas.push(BlockDelta {
                block: bundle.epoch,
                per_segment: bundle.per_segment_row_deltas,
            });
        }
        let ikpir_coalesced = BlockDelta::coalesce(&per_mutation_deltas).unwrap();

        let risepir_store =
            Segmented2aryCuckooKVStore::from_cells(cells0.clone(), params0, n0).unwrap();
        let mut risepir: RisePirServer<Segmented2aryScheme, SimplePirBackend> =
            RisePirServer::new(risepir_store, cfg.clone(), codec, 0);
        let genesis_bundle = risepir.setup();

        let n = changes.len() as u64;
        let risepir_delta = risepir
            .apply_block(&BlockUpdate {
                block: n,
                changes: changes.clone(),
                credits: vec![],
            })
            .unwrap();

        assert!(
            risepir_delta.per_segment.iter().any(|seg| !seg.is_empty()),
            "sanity: the compared delta must be non-empty, or the equality check below is vacuous"
        );
        assert_eq!(
            ikpir_coalesced.per_segment, risepir_delta.per_segment,
            "batched fold must produce the identical sparse delta transcript as N per-mutation folds \
             — if this differs, either this crate's `fold` reimplementation or `apply_block`'s \
             per-change plumbing has diverged from upstream's semantics"
        );
        assert_eq!(ikpir_coalesced.block, risepir_delta.block);

        // ── Claim 2: hint-patch linearity, within RisePirServer's own
        // randomness (see the doc comment above for why not cross-system). ──
        let final_bundle = risepir.setup();

        let mut replay_store = Segmented2aryCuckooKVStore::from_cells(cells0, params0, n0).unwrap();
        replay_store.enable_mutation_log();
        let _ = replay_store.drain_mutations();

        let material: Vec<_> = genesis_bundle
            .backend_params
            .iter()
            .map(SimplePirBackend::expand_hint_material)
            .collect();
        let mut replay_hints = genesis_bundle.hints.clone();

        for (a, bal) in &changes {
            let v = value_codec().encode(a, *bal).unwrap();
            replay_store.update(a, &v).unwrap();
            let muts = replay_store.drain_mutations();
            let row_deltas = fold_mutations_into_row_deltas(&muts, &params0);
            for (j, deltas) in row_deltas.iter().enumerate() {
                if !deltas.is_empty() {
                    SimplePirBackend::server_patch_hint(
                        &genesis_bundle.backend_params[j],
                        &material[j],
                        &mut replay_hints[j],
                        deltas,
                        HintPatchMode::EntryLevel,
                    );
                }
            }
        }

        assert!(
            replay_hints
                .iter()
                .zip(&genesis_bundle.hints)
                .any(|(patched, genesis)| patched.data != genesis.data),
            "sanity: at least one segment's hint must actually change from genesis, or the \
             bit-identity check below is vacuous"
        );
        for (j, (replayed, final_hint)) in replay_hints.iter().zip(&final_bundle.hints).enumerate()
        {
            assert_eq!(
                replayed.data, final_hint.data,
                "segment {j}: one batched patch must be bit-identical to N sequential patches \
                 over the same starting hint and the same A"
            );
        }
    }

    // ── Extra: failure drains the log so nothing leaks into the next
    // block (the precedent `apply_block`'s docs cite: `IkpirServer`'s own
    // `mutation_log_drained_on_failure` test). Not one of this crate's 7
    // named required tests, but directly exercises the "at minimum,
    // drain the mutation log so no partial deltas leak" requirement. ──

    /// A block that fails partway through (here: one change succeeds,
    /// the next overflows) must not let the successful prefix's mutation
    /// leak into a *later* block's delta. Verified by comparing the next
    /// successful block's delta against an oracle server that only ever
    /// saw that later block's own change — if the prefix leaked, the two
    /// deltas would differ (the real server's would additionally contain
    /// the leaked address's row).
    #[test]
    fn apply_block_failure_drains_log_so_no_leak_into_next_block() {
        let geom = geometry();
        let codec = value_codec();
        let cfg = config();

        let mut genesis_store = empty_store(&geom);
        genesis_store
            .insert(addr(0), &codec.encode(&addr(0), 1_000u128).unwrap())
            .unwrap();
        genesis_store
            .insert(addr(1), &codec.encode(&addr(1), 2_000u128).unwrap())
            .unwrap();
        let cells0 = genesis_store.snapshot_cells();
        let params0 = genesis_store.params();
        let n0 = genesis_store.num_items();

        let mut s: RisePirServer<Segmented2aryScheme, SimplePirBackend> = RisePirServer::new(
            Segmented2aryCuckooKVStore::from_cells(cells0.clone(), params0, n0).unwrap(),
            cfg.clone(),
            codec,
            0,
        );

        // Block 1: addr(0)'s update succeeds first (reaching the store's
        // cells), then addr(999)'s balance overflows — the whole block
        // is rejected.
        let overflow = 1u128 << BALANCE_BITS;
        let err = s
            .apply_block(&BlockUpdate {
                block: 1,
                changes: vec![(addr(0), 5_000u128), (addr(999), overflow)],
                credits: vec![],
            })
            .unwrap_err();
        assert_eq!(err, ServerError::Encode(ValueError::BalanceOverflow));
        assert_eq!(
            s.block(),
            0,
            "failed block 1 must not advance the block counter"
        );

        // Block 2: only addr(1) changes.
        let delta2 = s
            .apply_block(&BlockUpdate {
                block: 2,
                changes: vec![(addr(1), 6_000u128)],
                credits: vec![],
            })
            .unwrap();

        // Oracle: a server that only ever saw the original genesis (no
        // addr(0) tampering) and this one addr(1) change. addr(1)'s fold
        // output depends only on its own old→new transition, which is
        // identical in both scenarios, so an honest implementation's
        // `delta2` must match this exactly.
        let mut oracle: RisePirServer<Segmented2aryScheme, SimplePirBackend> = RisePirServer::new(
            Segmented2aryCuckooKVStore::from_cells(cells0, params0, n0).unwrap(),
            cfg,
            value_codec(),
            0,
        );
        let oracle_delta = oracle
            .apply_block(&BlockUpdate {
                block: 1,
                changes: vec![(addr(1), 6_000u128)],
                credits: vec![],
            })
            .unwrap();

        assert_eq!(
            delta2.per_segment, oracle_delta.per_segment,
            "block 2's delta must reflect only addr(1)'s change — not a leaked copy of \
             addr(0)'s change from the failed block 1"
        );
    }

    // ── 3. balance_overflow_is_rejected ─────────────────────────────────

    /// A balance `>= 2^balance_bits` must return `Err` and never
    /// truncate; the server's block must not advance.
    #[test]
    fn balance_overflow_is_rejected() {
        let geom = geometry();
        let mut s = server(&geom);
        let overflow_balance: Balance = 1u128 << BALANCE_BITS;
        let update = BlockUpdate {
            block: 1,
            changes: vec![(addr(0), overflow_balance)],
            credits: vec![],
        };

        let err = s.apply_block(&update).unwrap_err();
        assert_eq!(err, ServerError::Encode(ValueError::BalanceOverflow));
        assert_eq!(
            s.block(),
            0,
            "a rejected block must not advance the server's block"
        );

        // The boundary value itself must still succeed, proving this
        // isn't an off-by-one rejecting valid balances too.
        let ok_update = BlockUpdate {
            block: 1,
            changes: vec![(addr(0), overflow_balance - 1)],
            credits: vec![],
        };
        assert!(s.apply_block(&ok_update).is_ok());
    }

    // ── delete-on-zero (ADR-0015) ───────────────────────────────────────

    /// A `balance == 0` change for a key that was never inserted must be a
    /// no-op: `Ok`, an empty delta, and the block still advances — never an
    /// error. An absent account already answers `0x0` by absence, so
    /// "setting it to zero" changes nothing.
    #[test]
    fn delete_absent_key_is_noop() {
        let geom = geometry();
        let mut s = server(&geom);
        let delta = s
            .apply_block(&BlockUpdate {
                block: 1,
                changes: vec![(addr(99_999), 0)],
                credits: vec![],
            })
            .unwrap();
        assert!(
            delta.per_segment.iter().all(|seg| seg.is_empty()),
            "a no-op delete must produce an empty delta"
        );
        assert_eq!(
            s.block(),
            1,
            "even a no-op block advances the block counter"
        );
    }

    /// `balance == 0` deletes a present key. Insert K, delete it, then
    /// delete it again: the first delete changes cells; the second is a
    /// no-op (K is already gone), proving the first actually *removed* K
    /// rather than storing a zero value. (The full client-level proof that
    /// a deleted key reads back as `NotFound` — which distinguishes a real
    /// delete from an update-to-zero — lives in `risepir-feed`'s `pipeline`
    /// integration test, which has a client; this crate cannot.)
    #[test]
    fn delete_on_zero_removes_key() {
        let geom = geometry();
        let mut s = server(&geom);
        let k = addr(7);

        s.apply_block(&BlockUpdate {
            block: 1,
            changes: vec![(k, 5_000_000_000_000_000_000u128)],
            credits: vec![],
        })
        .unwrap();

        let d2 = s
            .apply_block(&BlockUpdate {
                block: 2,
                changes: vec![(k, 0)],
                credits: vec![],
            })
            .unwrap();
        assert!(
            d2.per_segment.iter().any(|seg| !seg.is_empty()),
            "deleting a present key must change cells"
        );

        let d3 = s
            .apply_block(&BlockUpdate {
                block: 3,
                changes: vec![(k, 0)],
                credits: vec![],
            })
            .unwrap();
        assert!(
            d3.per_segment.iter().all(|seg| seg.is_empty()),
            "re-deleting an already-absent key must be a no-op (empty delta)"
        );
        assert_eq!(s.block(), 3);
    }

    // ── 4. delta_ring_window ─────────────────────────────────────────────

    /// A range fully inside the retention window returns the coalesced
    /// delta; a range reaching outside it returns `None`.
    #[test]
    fn delta_ring_window() {
        use crate::delta_ring::DeltaRing;

        let geom = geometry();
        let mut s = server(&geom);
        let mut ring = DeltaRing::new(5);
        let mut deltas = Vec::new();

        for block in 1u64..=8 {
            let changes = vec![(
                addr(block),
                42_000_000_000_000_000_000u128 + u128::from(block),
            )];
            let d = s
                .apply_block(&BlockUpdate {
                    block,
                    changes,
                    credits: vec![],
                })
                .unwrap();
            ring.push(d.clone());
            deltas.push(d);
        }

        // Capacity 5 retains the last 5 pushes: blocks 4..=8.
        assert_eq!(ring.oldest(), Some(4));
        assert_eq!(ring.head(), Some(8));

        // Inside window: range(4, 8) == coalesce(blocks 5..=8).
        let expected = BlockDelta::coalesce(&deltas[4..8]).unwrap();
        assert_eq!(ring.range(4, 8), Some(expected));

        // Outside window: block 1's worth of history (needed to bridge
        // from_block=0) has already been evicted.
        assert_eq!(ring.range(0, 8), None);
    }

    // ── 5. coalesced_delta_is_magnitude_bounded ─────────────────────────

    /// Over 200 blocks of churn, every coalesced delta cell must satisfy
    /// `|Δ| < 2^plaintext_bits` — the invariant that licenses the compact
    /// varint/zigzag wire codec (`docs/verification.md`,
    /// "coalesced deltas are magnitude-bounded").
    #[test]
    fn coalesced_delta_is_magnitude_bounded() {
        let geom = geometry();
        let mut s = server(&geom);
        let pool: Vec<AddressHash> = (0..64u64).map(addr).collect();
        let mut rng = Xorshift64(0xDEAD_BEEF_CAFE_F00D);

        let mut deltas = Vec::new();
        for block in 1u64..=200u64 {
            let changes: Vec<(AddressHash, Balance)> = (0..10)
                .map(|_| {
                    let idx = (rng.next() % pool.len() as u64) as usize;
                    let balance = u128::from(rng.next()) % (1u128 << BALANCE_BITS);
                    (pool[idx], balance)
                })
                .collect();
            let d = s
                .apply_block(&BlockUpdate {
                    block,
                    changes,
                    credits: vec![],
                })
                .unwrap();
            deltas.push(d);
        }

        let bound: u64 = 1u64 << geom.plaintext_bits;
        let check_bounded = |d: &BlockDelta| {
            for seg in &d.per_segment {
                for (_, cells) in seg {
                    for (_, delta) in cells {
                        assert!(
                            delta.unsigned_abs() < bound,
                            "|delta|={} exceeds bound 2^{}={bound}",
                            delta.unsigned_abs(),
                            geom.plaintext_bits
                        );
                    }
                }
            }
        };

        // The full 200-block coalesce is the headline check...
        check_bounded(&BlockDelta::coalesce(&deltas).unwrap());
        // ...plus a couple of sub-windows for extra coverage of the
        // telescoping property at different spans.
        check_bounded(&BlockDelta::coalesce(&deltas[0..50]).unwrap());
        check_bounded(&BlockDelta::coalesce(&deltas[100..200]).unwrap());
    }

    // ── 6. answer_does_not_require_client_epoch ─────────────────────────

    /// A query built against an old server state must still be answered
    /// `Ok` after the server has advanced many blocks — contrast
    /// `IkpirServer::answer`, which would return `StaleEpoch` in the
    /// analogous situation.
    #[test]
    fn answer_does_not_require_client_epoch() {
        let geom = geometry();
        let mut s = server(&geom);

        let bundle = s.setup();
        let mut states: Vec<_> = bundle
            .backend_params
            .iter()
            .zip(&bundle.hints)
            .map(|(p, h)| SimplePirBackend::client_setup(p, h))
            .collect();
        let queries: Vec<SimpleQuery> = states
            .iter_mut()
            .map(|st| SimplePirBackend::client_query(st, 0))
            .collect();

        for block in 1u64..=50 {
            let changes = vec![(
                addr(block),
                7_000_000_000_000_000_000u128 + u128::from(block),
            )];
            s.apply_block(&BlockUpdate {
                block,
                changes,
                credits: vec![],
            })
            .unwrap();
        }

        let (responses, answered_at) = s
            .answer(&queries)
            .expect("answer must succeed regardless of how stale the query's construction is");
        assert_eq!(responses.len(), geom.arity as usize);
        assert_eq!(
            answered_at, 50,
            "the response must name the server's actual head"
        );
    }

    // ── 7. malformed_query_rejected ─────────────────────────────────────

    /// Wrong segment count must produce a clean `Err`, never a panic.
    #[test]
    fn malformed_query_rejected() {
        let geom = geometry();
        let s = server(&geom);

        let empty: Vec<SimpleQuery> = Vec::new();
        assert_eq!(
            s.answer(&empty).unwrap_err(),
            ServerError::MalformedQuery {
                expected: geom.arity as usize,
                got: 0
            }
        );

        // Also check a too-many-segments query, built from real (valid)
        // queries duplicated, to rule out an off-by-one that only
        // catches "too few".
        let bundle = s.setup();
        let mut states: Vec<_> = bundle
            .backend_params
            .iter()
            .zip(&bundle.hints)
            .map(|(p, h)| SimplePirBackend::client_setup(p, h))
            .collect();
        let mut too_many: Vec<SimpleQuery> = states
            .iter_mut()
            .map(|st| SimplePirBackend::client_query(st, 0))
            .collect();
        too_many.push(too_many[0].clone());
        assert_eq!(
            s.answer(&too_many).unwrap_err(),
            ServerError::MalformedQuery {
                expected: geom.arity as usize,
                got: geom.arity as usize + 1
            }
        );
    }

    // ── compile-time Send + Sync assertion ──────────────────────────────

    /// `docs/plan.md` ADR-0010: the concrete types are auto-`Send + Sync`,
    /// so `RwLock<RisePirServer<..>>` must compile and work across
    /// threads. The assertions themselves run at compile time (a `const
    /// fn` bounded by `T: Send` cannot even be named if `T` is not
    /// `Send`); this is a `#[test]` so it is discoverable and reported by
    /// name in `cargo test` output, per this crate's spec.
    #[test]
    fn risepir_server_is_send_and_sync() {
        assert_send::<RisePirServer<Segmented2aryScheme, SimplePirBackend>>();
        assert_sync::<RisePirServer<Segmented2aryScheme, SimplePirBackend>>();
    }
}

#[cfg(test)]
mod verified_apply_tests {
    //! End-to-end `apply_block` behavior under engineered fingerprint
    //! collisions (ADR-0017) and withdrawal credits — the server-level
    //! counterpart of `crate::verified`'s store-level tests.

    use super::*;
    use ikpir_common::backend::simple::SimpleParams;
    use ikpir_common::pir_params::simple_max_plaintext_bits;
    use ikpir_common::{SimpleConfig, SimplePirBackend};
    use risepir_proto::{AddressHash, Balance, ValueError};
    use segmented_cuckoo::{CuckooParams, Segmented2aryCuckooKVStore, Segmented2aryScheme};
    use std::collections::HashMap;

    const NUM_BUCKETS: u32 = 2 * 8; // tiny: keeps the birthday search ~2^18
    const BUCKET_SIZE: u32 = 4;
    const FP_BITS: u32 = 32;
    const BALANCE_BITS: u32 = 96;

    fn codec() -> ValueCodec {
        ValueCodec {
            key_tag_bits: 32,
            balance_bits: BALANCE_BITS,
            checksum_bits: 16,
        }
    }

    fn tiny_server() -> RisePirServer<Segmented2aryScheme, SimplePirBackend> {
        let codec = codec();
        let pb = simple_max_plaintext_bits(
            2,
            NUM_BUCKETS / 2,
            BUCKET_SIZE,
            FP_BITS,
            codec.value_bits(),
            SimpleParams::DEFAULT_SIGMA,
        );
        let store = Segmented2aryCuckooKVStore::new(
            NUM_BUCKETS,
            BUCKET_SIZE,
            FP_BITS,
            codec.value_bits(),
            pb,
        )
        .unwrap();
        RisePirServer::new(store, SimpleConfig::with_lwe_dim(256), codec, 0)
    }

    fn addr(i: u64) -> AddressHash {
        let mut a = [0u8; 32];
        a[..8].copy_from_slice(&i.to_le_bytes());
        a
    }

    /// Same deterministic search as `verified::tests::colliding_pair`.
    fn colliding_pair(params: &CuckooParams) -> (AddressHash, AddressHash) {
        let mut seen: HashMap<(u64, u32), u64> = HashMap::new();
        for i in 0u64..30_000_000 {
            let a = addr(i);
            let (fp, idx) = params.candidate_buckets(&a);
            if let Some(&j) = seen.get(&(fp, idx[0])) {
                return (addr(j), a);
            }
            seen.insert((fp, idx[0]), i);
        }
        panic!("no (fingerprint, first-bucket) collision found in 30M keys");
    }

    fn block(n: u64, changes: Vec<(AddressHash, Balance)>) -> BlockUpdate {
        BlockUpdate {
            block: n,
            changes,
            credits: vec![],
        }
    }

    /// The regression ADR-0017 exists for: a `(A, 0)` change for an
    /// *absent* A whose fingerprint collides with a present foreign B must
    /// be a no-op — never a delete of B's entry (which is what the store's
    /// own fp-only `delete` does; pinned in `verified`'s tests).
    #[test]
    fn zero_change_for_absent_colliding_key_is_noop() {
        let mut s = tiny_server();
        let (a, b) = colliding_pair(&s.params());
        let b_balance: Balance = 42_000_000_000_000_000_000u128;

        s.apply_block(&block(1, vec![(b, b_balance)])).unwrap();
        let delta = s.apply_block(&block(2, vec![(a, 0)])).unwrap();

        assert!(
            delta.per_segment.iter().all(|seg| seg.is_empty()),
            "the (A, 0) no-op must produce an empty delta"
        );
        assert_eq!(s.block(), 2, "the no-op block still advances the head");
        assert_eq!(
            s.balance_of(&b).unwrap(),
            Some(b_balance),
            "B's entry must survive a zero-change for its fp-colliding absent neighbour"
        );
        assert_eq!(s.balance_of(&a).unwrap(), None);
    }

    /// Both colliding keys can coexist (insert only writes empty slots),
    /// verified reads disambiguate them, and a write to the *shadowed* one
    /// fails loudly with `FingerprintAmbiguity` instead of corrupting the
    /// foreign entry — then succeeds once the shadow is gone.
    #[test]
    fn shadowed_write_fails_loudly_then_recovers() {
        let mut s = tiny_server();
        let (a, b) = colliding_pair(&s.params());
        let b_balance: Balance = 7_000_000_000u128;
        let a_balance: Balance = 9_999_999_999u128;

        s.apply_block(&block(1, vec![(b, b_balance)])).unwrap();
        // A is Absent (foreign fp-match present) → verified dispatch takes
        // the insert path; both now coexist in the shared bucket.
        s.apply_block(&block(2, vec![(a, a_balance)])).unwrap();
        assert_eq!(s.balance_of(&a).unwrap(), Some(a_balance));
        assert_eq!(s.balance_of(&b).unwrap(), Some(b_balance));

        // A is Shadowed (B sits earlier in probe order): update and delete
        // must both reject the block loudly, changing nothing.
        let err = s.apply_block(&block(3, vec![(a, 1u128)])).unwrap_err();
        assert_eq!(err, ServerError::FingerprintAmbiguity { shadowed: true });
        let err = s.apply_block(&block(3, vec![(a, 0)])).unwrap_err();
        assert_eq!(err, ServerError::FingerprintAmbiguity { shadowed: true });
        assert_eq!(s.block(), 2, "rejected blocks must not advance the head");
        assert_eq!(
            s.balance_of(&a).unwrap(),
            Some(a_balance),
            "A untouched after rejects"
        );
        assert_eq!(
            s.balance_of(&b).unwrap(),
            Some(b_balance),
            "B untouched after rejects"
        );

        // B is Own (first match) — deleting it is safe, and un-shadows A.
        s.apply_block(&block(3, vec![(b, 0)])).unwrap();
        assert_eq!(s.balance_of(&b).unwrap(), None);
        assert_eq!(s.balance_of(&a).unwrap(), Some(a_balance));
        s.apply_block(&block(4, vec![(a, 0)])).unwrap();
        assert_eq!(s.balance_of(&a).unwrap(), None);
    }

    /// Withdrawal credits: relative amounts, resolved against the store's
    /// verified prior *after* the block's absolute changes; duplicates
    /// accumulate; a fresh recipient starts from zero.
    #[test]
    fn credits_apply_after_changes_and_accumulate() {
        let mut s = tiny_server();
        let (k1, k2) = (addr(1), addr(2));

        // Fresh recipient: two credits in one block accumulate from 0.
        s.apply_block(&BlockUpdate {
            block: 1,
            changes: vec![(k1, 100u128)],
            credits: vec![(k2, 50u128), (k2, 25u128)],
        })
        .unwrap();
        assert_eq!(s.balance_of(&k1).unwrap(), Some(100));
        assert_eq!(s.balance_of(&k2).unwrap(), Some(75));

        // Credit stacks on top of the same block's absolute change.
        s.apply_block(&BlockUpdate {
            block: 2,
            changes: vec![(k1, 200u128)],
            credits: vec![(k1, 5u128)],
        })
        .unwrap();
        assert_eq!(s.balance_of(&k1).unwrap(), Some(205));
    }

    /// A credit that would overflow `balance_bits` (or `u128` itself)
    /// rejects the whole block — never wraps, never truncates — and the
    /// prior state survives.
    #[test]
    fn credit_overflow_rejects_block() {
        let mut s = tiny_server();
        let k = addr(1);
        s.apply_block(&block(1, vec![(k, 205u128)])).unwrap();

        // 205 + (2^96 - 1) >= 2^96: encode-level BalanceOverflow.
        let err = s
            .apply_block(&BlockUpdate {
                block: 2,
                changes: vec![],
                credits: vec![(k, (1u128 << BALANCE_BITS) - 1)],
            })
            .unwrap_err();
        assert_eq!(err, ServerError::Encode(ValueError::BalanceOverflow));

        // 205 + u128::MAX overflows u128 itself: checked_add-level.
        let err = s
            .apply_block(&BlockUpdate {
                block: 2,
                changes: vec![],
                credits: vec![(k, u128::MAX)],
            })
            .unwrap_err();
        assert_eq!(err, ServerError::Encode(ValueError::BalanceOverflow));

        assert_eq!(s.block(), 1);
        assert_eq!(
            s.balance_of(&k).unwrap(),
            Some(205),
            "prior state survives the rejects"
        );
    }
}
