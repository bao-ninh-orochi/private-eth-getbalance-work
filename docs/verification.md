# Verification log — the brief's claims vs. the code

> **Status: this is the evidence log, not the current spec.** It records what was
> measured and checked against the IKPIR source. The measured numbers, the closed-form
> reproductions, the rewind/batching/Frodo/withdrawal findings, and the Sepolia-scale
> facts all remain valid. Where it discusses *Sepolia-first* or a *bounded universe*,
> those framings are **superseded** — the project now targets **mainnet** with a
> **complete nonzero-balance set** (see [`plan.md`](plan.md), ADR-0013/0015/0016).
> [`plan.md`](plan.md) is the authoritative current spec.


The brief says: *"Treat everything in this section as a prior to check, not fact.
Re-derive before you rely on it."* This is that re-derivation.

Method: every claim was checked against `Incremental-Keyword-PIR` @ `main`
(`3287c39`) and `perf/optimized` (`042d868`), by reading the source, running the
tests, and reproducing the numbers with an independent implementation of the
closed forms. Measurements are on an 8-core Apple Silicon laptop, 16 GB RAM,
`perf/optimized` + `-C target-cpu=native`.

**Bottom line: the brief is substantially accurate. Five corrections follow, two
of which change the plan.**

---

## ✅ Confirmed exactly

| Claim | Where | Result |
|---|---|---|
| Strict `q.epoch == self.epoch` check in `answer` | `server.rs:253` | Confirmed |
| `commit_mutations` bumps epoch unconditionally (⇒ ~300 epochs/block) | `server.rs:583` | Confirmed |
| `fold_mutations_into_row_deltas` folds many mutations into one sparse delta | `hint_patch.rs:65` | Confirmed |
| `two_mutations_same_slot_sum_correctly` exists | `hint_patch.rs:209` | Confirmed |
| `apply_delta` demands `delta.epoch == self.epoch + 1` | `client.rs:439` | Confirmed |
| `PreparedSlot` is private in both backends | `frodo/backend.rs:136`, `simple/backend.rs:170` | Confirmed |
| FIFO, ≤1 in-flight query per segment | `backend/mod.rs:315-319` | Confirmed |
| Delta wire cost is 10 B/cell (`u16` offset + `i64` delta) | `wire.rs:296` | Confirmed |
| No serde anywhere; wire types all `pub` | `wire.rs:13-18` | Confirmed |
| `setup`/`answer` are `&self`; read path never touches `HintMaterial` | `server.rs:208,248` | Confirmed |
| `num_buckets` must be `2^t` (2/4-ary) or `3·2^t` (3-ary) | `store.rs:653,788,904` | Confirmed |
| `perf/optimized` = strict descendant of `main`, 10 commits, no API change | `git merge-base` | Confirmed |
| `perf/optimized` carries `target-cpu=native` a git dep won't inherit | `.cargo/config.toml` | Confirmed |
| **Response-rewind identity holds** | `tests/response_rewind.rs` | **8/8 pass in 0.02 s** |
| SimplePIR reshape rule `k = max(1, round(√(n_rows/row_width)))` | `simple/backend.rs:461` | Confirmed — and it is *documented* in `pir_params.rs`, not merely inferrable |

### §4 closed forms reproduce the measured CSVs exactly

An independent implementation of the closed forms reproduces every value in
`results/*.csv` for the one measured config (`arity 4 / 65536 / bs 4 / 256-bit values`):

| Quantity | Derived | CSV |
|---|---|---|
| frodo `pb` / `cells_per_slot` / `row_width` | 11 / 27 / 108 | 11 / 27 / 108 |
| frodo hint/segment | 676,512 B | 676,512 |
| frodo query bundle / response bundle | 262,156 / 1,740 B | 262,156 / 1,740 |
| simple `pb` / `cells_per_slot` / `row_width` | 10 / 29 / 116 | 10 / 29 / 116 |
| simple `k` / `R` / `C` | 12 / 1,366 / 1,392 | `db_rows`=1366, `db_cols`=1392 |
| simple hint/segment | 7,099,200 B | 7,099,200 |
| simple query / response bundle | 21,868 / 22,284 B | 21,868 / 22,284 |

The mainnet extrapolation also reproduces: S hint 708.9 MB (brief: ~710), q/r
556 KB (~557), A 709 MB (~710); F hint 1.2 MB, F query 402.7 MB (~403), F client
`A` 210.2 GB/segment (~210); server DB 25.8 GB (~26); `pb`=8, `row_width`=64. ✅

---

## ❌ Correction 1 — the SimplePIR closed forms have R and C swapped

The brief's §4 says:

```
SimplePIR: hint = R × lwe_dim × 4 ;  query = C × 4 ;  response = R × 4 ;  A = C × lwe_dim × 4
```

The code says the opposite in all four:

```
hint     = lwe_dim × C × 4     (SimpleHint.data is lwe_dim × reshape_row_width)
query    = R × 4               (SimpleQuery.b   has length reshape_rows)
response = C × 4               (SimpleResponse.a has length reshape_row_width)
A        = R × lwe_dim × 4     (SimpleHintMaterial.a is reshape_rows × lwe_dim)
```

with `k = max(1, round(√(n_rows/row_width)))`, `R = ⌈n_rows/k⌉`, `C = k·row_width`.

**Impact: numerically invisible, because the reshape is near-square** (at mainnet
R = 46,346 vs C = 46,336 — 0.02% apart). That is exactly why inferring the rule
from hint sizes appeared to work. It matters only for exact sizing and for anyone
reasoning about the asymmetric cases. The brief's conclusions are unaffected.

## ❌ Correction 2 — **no upstream changes are needed** (the brief says two are)

§8 claims two things "genuinely need upstream changes": (1) `apply_delta`
accepting a multi-epoch jump, and (2) the rewind, because `PreparedSlot` is
private so `client_decode` must change in-crate.

**Both are avoidable, and I have proven it by running the full path from outside
the crate.** The `IndexPirBackend` / `IncrementalPirBackend` trait methods are all
`pub`, and `ServerSetupBundle.{backend_params,hints}` are `pub`. So one drives:

```rust
let mut states: Vec<B::ClientState> = bundle.backend_params.iter().zip(&bundle.hints)
    .map(|(p, h)| B::client_setup(p, h)).collect();
let q    = B::client_query(&mut states[j], row);      // no IkpirClient, no epoch state machine
let cells= B::client_decode(&states[j], &corrected);  // raw bucket cells — what step 2 needs
B::client_patch_state(&mut states[j], &coalesced, mode);  // no epoch+1 check to fight
```

- (1) evaporates: `apply_delta`'s epoch+1 check lives in `IkpirClient`, not in the
  backend. Calling `B::client_patch_state` directly applies an arbitrarily
  coalesced delta. **Verified**: a single delta coalescing 500 blocks (10,500
  server epochs) applied in one shot, then decoded correctly.
- (2) evaporates: the response correction needs only `FrodoQuery.b` /
  `FrodoResponse.a` / `SimpleQuery.b` / `SimpleResponse.a` / `SimpleServerParams.{k,row_width}`
  — all `pub`. `PreparedSlot` is never touched. The crate's own
  `tests/response_rewind.rs` already proves this: an integration test *is* an
  external crate.

The cost is reimplementing the ~100-line `IkpirClient` wrapper, which we need
anyway (see Correction 3). **This removes the project's only external
coordination dependency.**

## ❌ Correction 3 — the *real* upstream gap is a batch-mutation API

The brief looked for upstream changes in the wrong place. `IkpirServer` has **no
batched mutation entry point**: `insert`/`update`/`delete` each call
`commit_mutations()`, which drains, folds, patches, and bumps the epoch. A
300-change block therefore does 300 folds, 300 hint patches, and 300 epochs — the
very thing §5 says we don't want.

`mod hint_patch` is private, so `fold_mutations_into_row_deltas` is not reachable;
but it is ~50 lines over entirely public types (`SlotMutation` fields,
`pack_slot_cells`, `CuckooParams`), and `store.as_cells()` /
`enable_mutation_log()` / `drain_mutations()` are all `pub`. So a batched server
is buildable outside the crate too.

Measured (SimplePIR, 3-ary, 12-byte balances, 300 updates/block):

| seg_rows | A) 300× `IkpirServer::update` | B) batched: 1 drain + 1 fold + 1 patch/seg | speedup |
|---:|---:|---:|---:|
| 16,384 | 3.134 ms | 1.477 ms | **2.12×** |
| 65,536 | 5.378 ms | 2.328 ms | **2.31×** |

(At 262,144 the in-process comparison is confounded by holding two databases at
once; the semantic argument for batching — one epoch and one delta bundle per
block — is independent of the speedup.)

**Plan impact:** build our own batched server over the public primitives, and
offer *this* upstream as the small PR, in place of the two the brief proposed. It
is motivated by a measured problem rather than an anticipated one.

### A third upstream gap: `server_setup` samples its seed with no injection point

Surfaced while writing `batched_equals_per_mutation`. `SimplePirBackend::server_setup`
(and Frodo's) samples a fresh random seed for `A` via `rand::rng()` on every call,
and there is **no public way to inject a fixed seed**, nor any constructor taking a
pre-built `(ServerParams, Hint)`. So two independently-constructed servers get
different `A` and different hint bytes **by construction**, regardless of
correctness — making a cross-server hint comparison untestable (it would only be
testing whether two random draws collided).

Consequences beyond testing: setup is **not reproducible**, so a server cannot be
rebuilt bit-identically from the same database, and a client cannot verify a hint it
was handed. `reset_for_replay` exists as a bench/test escape hatch precisely because
of this, but it is not a general answer.

Workaround used: prove (a) delta-transcript equivalence against a real `IkpirServer`
oracle — seed-independent, since the fold is a pure function of the mutations — and
(b) hint-patch linearity (one batched patch vs. N sequential) replayed within one
server's *own* captured seed, via `expand_hint_material`'s documented determinism.
That is the strongest statement the current API admits.

**Upstream PR candidate #2**, alongside the batch API: accept an optional seed in
`Config`. Small, and it buys reproducibility, not just testability.

## ❌ Correction 4 — per-block patch is **not** constant in wall-clock

§1/§4 claim ~2.4 ms/block "with **no dependence on database size**". The MAC count
is indeed N-independent (`mutations × cells × lwe_dim`). The wall-clock is not:

| seg_rows | accounts | patch/block |
|---:|---:|---:|
| 4,096 | 36,864 | 1.175 ms |
| 65,536 | 589,824 | 3.213 ms |
| 1,048,576 | 9,437,184 | 4.688 ms |

Cause: the entry-level patch does `H[k'][c] += A[r][k']·γ` for all `k' ∈ [n]` — a
**strided** walk down column `c` of a `lwe_dim × C` row-major hint. Stride is
`C·4` bytes and `C ≈ √(n_rows·row_width)` grows with N, so the hint stops fitting
in cache (4.8 → 10.1 → 20.2 MB per segment across the rows above). It is
bandwidth-bound, not FLOP-bound, and should **plateau** (not grow without bound)
once the hint is fully out of cache — one cache line per hint element touched,
independent of C.

The headline survives easily (4.7 ms vs a 12,000 ms block budget), but the number
must be reported as measured, and the claim should be stated as *"N-independent in
op count; constant in wall-clock once the hint exceeds cache"*.

## ❌ Correction 5 — **Frodo is dead at Sepolia scale too**, not just mainnet

§4/§10 say *"at Sepolia scale Frodo may well win — a 1.2 MB constant hint is
extremely attractive for a wallet"*, confidence "low for Sepolia — measure both".

Measured and derived: at 9.4M accounts (`seg_rows = 2^20`, 3-ary, bs 4, 12-byte
balances), the FrodoPIR **client** must hold `A = n_rows × lwe_dim × 4` per
segment = 6.6 GB × 3 = **19.7 GB**. `A` is needed in full for every query
(`b = A·s + e + Δ·u_row` touches every row), so it cannot be sampled lazily. The
1.2 MB hint is irrelevant — the client is dead on `A` before the hint matters.
Frodo's *rebuild* is also ~5× slower than Simple's at equal N (756 ms vs 138 ms at
`seg_rows = 65536`), because `Aᵀ·D` is a much larger GEMM at `lwe_dim = 1566`
with no reshape.

The corresponding SimplePIR client at the same scale needs `A` = 40.5 MB/seg and
hint = 40.4 MB/seg → **~243 MB RAM, ~121 MB one-time download** (`A` is
seed-derived and never on the wire).

**Decision: RisePIR-S. The evidence closes the question the brief left open.**
Frodo remains worth one row in the numbers table as the measured justification.

---

## 🔬 New findings the brief does not state

### The concrete types *are* `Send + Sync`

§8 says *"`ClientState`/`Query`/`Response` carry **no `Send`/`Sync` bounds**"* and
concludes "plan for one client instance per in-flight request". True at the
*trait* level, but the concrete types are auto-`Send + Sync` — verified by
compiling `assert_send::<IkpirClient<SimplePirBackend>>()` etc. for both backends,
both servers, and all four bundles.

So `RwLock<IkpirServer>` and `Mutex<IkpirClient>` both work across threads. The
real constraint is only `&mut self` + FIFO consumption — i.e. **strict lockstep
behind a mutex is sufficient**, and per-request client instances (which would each
carry hundreds of MB) are *not* required. This substantially simplifies the client.

### Coalesced deltas are magnitude-bounded — which licenses a much tighter codec

For any `(row, offset)`, a delta coalesced over `[E, E']` telescopes:
`Σ(c_{i+1} − c_i) = c_{E'} − c_E`. Both endpoints are real cell states in `[0, p)`,
so **|Δ| < p ≤ 2^14 no matter how many blocks are coalesced.** Coalescing is free
in delta magnitude.

Consequences:
- the `i64` in the wire format is ~4× wider than mathematically necessary;
- `varint(offset_gap) + zigzag_varint(Δ)` ≈ 3 B/cell vs 10 B/cell;
- and the bound is a **free integrity check**: assert `0 ≤ cell + Δ < p` when
  applying, and a whole class of pipeline bugs fails loudly instead of returning a
  plausible wrong balance.

Verified empirically: the assertion never fired across 13,220 delta cells / 500
blocks of churn. Measured compaction on synthetic data was only 1.66–1.93×,
because the test balances were small integers that changed few cells; realistic
wei-scale balances change ~11–15 cells per update and should approach the ~3×.
**The delta benchmark must use realistic balance data** — this is a live trap.

### `update` touches exactly one slot, and `pb | fingerprint_bits` is free money

`store.rs:1420` — `update` finds the key in one of its `d` candidate buckets and
rewrites that one slot, re-writing the *same* fingerprint. The fold drops zero
deltas, so **only changed value cells** are on the wire. Choosing `pb` such that
`fingerprint_bits % pb == 0` (e.g. `fp=32, pb=8`) keeps the fingerprint on exact
cell boundaries so it never contributes a delta; at `pb=9` the fingerprint
straddles a cell and every update pays one extra cell.

### The ordering trap is real, and now has a regression test

§6 warns that the `ΔD` correction must be applied to the recovered bucket **before**
the fingerprint scan. Reproduced: with a hint pinned 10,500 epochs stale and a key
inserted after the pin, scanning first returns `None` — i.e. **`0x0` for an account
that demonstrably exists**. Correcting first returns the right balance. Both
directions are pinned by a test.

### The value width is a "never return a wrong answer" hazard

12 bytes (96 bits) holds any *mainnet* balance (total supply ≈ 1.2e26 wei ≈ 2^86.6).
**Sepolia is a testnet with faucet-minted ETH and no supply discipline** — the
assumption may not hold. Silent truncation at ingest is exactly the failure the
invariant forbids, so ingest must hard-fail on `balance ≥ 2^value_bits` rather than
truncate, and the actual Sepolia maximum must be measured before the width is fixed.

### A decode failure returns `0x0`, not an error

If LWE noise corrupts a fingerprint cell, the scan misses → `None` → the RPC
answers `0x0` for an existing account. If it corrupts a *value* cell while the
fingerprint still matches, the RPC answers a **corrupted balance**. Neither is
detectable today: the fingerprint authenticates the key, nothing authenticates the
value. Rate is small (`δ = 2^-40` per cell by construction, and the `pb` selection
is worst-case so real margins are wider) but it is a silent-wrong-answer path, which
the invariant ranks above everything.

Mitigation, cheap and entirely in our layer since `value_bits` is ours to choose:
carry a checksum inside the value (`balance ‖ crc`). A mismatch then means "decode
failed" → return an error, not a number. Cost is `⌈crc_bits/pb⌉` extra cells per
slot (~+12% `row_width` for 16 bits at `pb=8`), paid on answer CPU and response
size. See ADR-0009.

---

## ❌ Correction 6 — **`prestateTracer` misses withdrawals**: the brief's fix for its own "biggest trap" does not work

§7 says: *"This is the single biggest trap. Deriving balance deltas from transaction
`value` fields will silently miss … **validator withdrawals — which credit balances
with no transaction at all**. Use `debug_traceBlock` with the `prestateTracer`, or
Erigon/Reth's `trace_block`, or Reth ExEx's `BundleState`."*

**Two of those three prescriptions do not fix the trap they are prescribed for.**
Verified empirically on Sepolia block `0xac2700` via a live endpoint:

| Signal | Result |
|---|---|
| tx traces returned | 128 |
| withdrawal recipient `0x89bb…8da1` present in any `pre`/`post` | **0 / 128** ❌ |
| fee recipient present in `post` | 128 / 128 ✅ |

The miss is **total**, not partial — that recipient had no transaction touching it:

```
sum(block.withdrawals[].amount) = 3,613,635 gwei = 3,613,635,000,000,000 wei
eth_getBalance @N − @N-1        = 3,613,635,000,000,000 wei      → EXACT MATCH
```

This is structural. EIP-4895 processes withdrawals *after* user-level transactions
as system-level operations, and `debug_traceBlockByNumber` returns **one entry per
transaction** — there is no slot in the response for them. The same applies to
Erigon/Parity `trace_replayBlockTransactions` `stateDiff` (also tx-scoped; its
`reward` trace type is for **PoW block rewards**, not withdrawals). Only the third
prescription — **ExEx `BundleState`** — captures them, because reth's executor
applies withdrawals in `apply_post_execution_changes()` into the same `State`.

**The correct RPC recipe:**

```
per_block_diff = prestateTracer(diffMode) over all txs      # value, internal CALLs, SELFDESTRUCT, gas, coinbase
               ⊕ eth_getBlockByNumber(...).withdrawals[]     # amount is in GWEI → ×10^9
```

Withdrawals come free from the block body — no tracing, no archive node.

**Second trap in the same API: `post` is sparse.** Only *changed* fields appear. An
address whose storage changed but whose balance did not has **no `balance` key in
`post`**. So:

```
new_balance(a) = post[a].balance ?? pre[a].balance      // absent ⇒ UNCHANGED, not zero
```

Reading absent-as-zero would zero out balances — a silent wrong answer, and an easy
bug to write. An address in `pre` but wholly absent from `post` = deleted.

## ❌ Correction 7 — **Sepolia is not small**; a node does not fit here

§10 says *"Sepolia first… Syncs in hours, stable"*, and §7 says *"For Sepolia this
is hours and modest disk"*, confidence High.

Measured directly (ranged GET on `Content-Range`, not read off a docs page):
PublicNode's **non-archive** Reth Sepolia snapshot
(`ethereum-sepolia-reth-11275037.tar.lz4`, `Last-Modified: 2026-07-15`, block
11,275,037) is **735,677,758,198 bytes = 735.7 GB compressed**. Uncompressed is
larger. Geth Sepolia full is listed at 791.7 GB.

Sepolia has been heavily spammed; **its non-archive footprint is in the same league
as mainnet's.** Reth's own docs publish no Sepolia figures at all (mainnet full:
≥1.2 TB). 16 GB RAM is fine (full wants 8 GB+); **disk is the binding constraint**,
and 303 GB is not close. A true `reth --full` (retains ~10,064 blocks of history)
should be smaller, by an unverified margin.

⇒ **Running a Sepolia node in this environment is off the table.** Not a scheduling
problem — a hardware one. This forces the staged-universe approach (ADR-0013).

## ❌ Correction 8 — **Sepolia is ~150M accounts**, not 1M–4M. It is mainnet-scale.

§10 says: *"Sepolia first, mainnet later — … its state should land in the **1M–4M
band** the paper already benchmarked — so your numbers cross-validate the paper on
real data."* Confidence: **High**.

Measured: **≈1.5 × 10⁸ accounts** (range 1.4–1.75 × 10⁸; hard bounds ≥3.25 × 10⁷,
≤2.47 × 10⁸), derived by `eth_getProof` trie-depth sampling and corroborated within
3% by Blockscout's `totalAccounts + totalContracts` = 150.3M. **Two orders of
magnitude above the brief's estimate, and half of mainnet.** Sepolia has been
heavily spammed — consistent with the 735.7 GB snapshot (Correction 7).

⚠️ Note the definitional trap that makes this easy to get wrong: Blockscout's
`totalAccounts` alone counts only **EOAs that sent ≥1 transaction**, not state
accounts. State accounts are strictly more.

**This inverts the framing of §1 — in our favour.** At 3-ary/`bucket_size` 4,
150M accounts lands at `num_buckets = 3·2²⁴` → 201M slots → **74.5% load exactly**
(the same clean fit the brief predicted for mainnet), `segment_rows = 2²⁴`:

| accounts | full rebuild (extrapolated from measured) | duty cycle @ 12 s block |
|---|---:|---:|
| 9.4M (measured) | 4.9 s | 41% — barely possible |
| **Sepolia ≈150M** | **79 s (linear) – 276 s (observed superlinear)** | **655% – 2302% → impossible** |
| mainnet ≈300M | 157 s – 757 s | 1310% – 6306% → impossible |

**The impossibility argument no longer needs a mainnet extrapolation.** It is
demonstrable on a live public testnet today: Sepolia is already ~7–23× over budget,
against a per-block patch duty cycle of ~0.08%. The crossover from "possible" to
"impossible" sits around ~20–30M accounts; Sepolia and mainnet are both well past
it. This is a strictly stronger claim than the brief's, and it is *measurable*
rather than projected.

Costs of the correction: a full-Sepolia server needs **DB 12.88 GB + hints 0.50 GB +
`A` 0.50 GB ≈ 13.9 GB** before the account map — so it does **not** fit this 16 GB
box. Stage 2 needs ~32–64 GB RAM, on top of the disk problem. And Sepolia does *not*
cross-validate the paper's 1M–4M benchmark band; that rationale for choosing Sepolia
is void (though better reasons remain: it is free, public, and now demonstrably past
the impossibility threshold).

## ❌ Correction 9 — Sepolia sees **~140** balance changes/block, not 10–50

§11 says: *"Sepolia's tx rate is far below mainnet, so you'll see maybe **10–50
changes per block** instead of ~300 — which *understates* your headline."*

Measured: **~140 balance-changing accounts/block** (p50 148, p90 ~170, max 176),
block time **12.3604 s** (over 100k blocks; matches Blockscout's 12.36 s exactly).
Xatu independently reports ~218 diff *events*/block and 95 distinct addresses in
block 5,000,000 — consistent once events vs. distinct addresses are separated.

So the understatement is ~2×, not ~10×. The patch-cost curve over mutations/block is
still the right measurement, but Sepolia's operating point is much closer to
mainnet's ~300 than the brief expected.

## 🔎 A public Sepolia balance dataset exists — and it replaces the RPC oracle

The brief's §10 lists as uncertain: *"Whether any public dataset can shortcut the
mainnet 300M-account bootstrap."* For **Sepolia**, one exists:

```
https://data.ethpandaops.io/xatu/sepolia/databases/default/
    canonical_execution_balance_diffs/1000/{block//1000*1000}.parquet
```

- **Coverage: blocks 1,066,000 → 10,073,999** (verified: chunk 1065000 is a
  340-byte empty file, 1066000 is real, 10074000 is a 404). One chunk = **1000
  consecutive blocks, ~218k rows, ~8.3 MB**.
- `to_value` is the **absolute post-tx balance**, not a delta — so
  `last to_value per address by (block_number, transaction_index, internal_index)`
  reconstructs state in one pass, with no replay.
- **Verified against live archive RPC: 12/12 exact balance matches** at block
  5,000,000, reproduced independently on dRPC and Tenderly (0 wei delta).

**This replaces the entire ~100k-call conformance oracle with one 8 MB download**,
provided the test window sits at blocks ≤10,073,999. Three traps, all confirmed by
testing, all silent-wrong-answer class:

1. **Values are 32-byte LITTLE-endian uint256.** Big-endian parses fine and yields
   ~1e76 — plausible-looking garbage.
2. **Take the LAST diff per address per block.** The first occurrence at block
   5,000,000 gives 911,999.122730 ETH vs the correct 911,999.128165467 — silently
   close enough to look right.
3. **Beacon withdrawal credits are ABSENT** — every row is tx-bound, and
   `canonical_beacon_block_withdrawal` has no Sepolia coverage. Confirmed by direct
   test: block 10,073,500 credits `0x89bb…8da1` by 87,495,000,000,000 wei, the RPC
   delta confirms the balance moved, and **the address is not in the parquet**. This
   is the *same* withdrawal gap as Correction 6, from a second, independent source.
   Bounded but dangerous: Sepolia's permissioned set means only **1–4 unique
   withdrawal addresses**, but they are among the **largest holders** (`0x89bb…`
   holds ~100M ETH, #2 on Blockscout). Must be patched from RPC.

Do **not** try to reconstruct *current* state from this: the 1.2M-block tail
(10,074,000 → head) would need `prestateTracer` on 1.2M blocks, which rate-limits
into days. Pick a window inside the covered range.

Operational notes: **PublicNode and 1rpc reject archive-height `eth_getBalance`**;
only **dRPC** and **Tenderly** served it keyless. Strategic caveat worth confirming
before building on Sepolia: its announced **2026 sunset** and completed history
expiry.

## 🔎 Ethereum-side findings the brief could not have known

- **`sepolia.drpc.org` is free, keyless, and serves `debug_traceBlockByNumber` +
  `prestateTracer`** (`web3_clientVersion` = `Geth/v10.0.0/drpc`). This is the
  Stage-1 feed.
- **Reth's `debug_accountRange` is a no-op stub** — declared `-> RpcResult<()>`,
  body is literally `Ok(())` (`crates/rpc/rpc/src/debug.rs:982-992`). It compiles,
  it responds, it returns nothing. The brief contrasts Reth/Erigon's flat state
  against "the hours Geth's `debug_accountRange` trie walk takes"; on Reth the
  method is not implemented at all.
- **Reth 2.x dropped `PlainAccountState`.** Storage v2 is the default and
  `migrate_v2` explicitly clears the table (*"Plain state — superseded by hashed
  state in v2"*). The classic "cursor-walk `PlainAccountState`" enumeration recipe
  is **dead on reth 2.x**; v2 gives you *hashed* keys.
  **This is good news for us, not bad.** The agent flagged it as "the preimage
  problem", but we key the SCF by `keccak256(address)` (§7 / ADR-0008) — so reth
  v2's `HashedAccountState` *is* our enumeration source, in exactly our key space,
  with no preimage needed. **The brief's keying decision turns reth v2's problem
  into a non-issue.** A nice, unplanned confirmation of that call.
- **Reth ExEx is current and has moved.** v2.4.0 (released 2026-07-14). Since 2024:
  `examples/exex/` **no longer exists in-tree** (moved to
  [`paradigmxyz/reth-exex-examples`](https://github.com/paradigmxyz/reth-exex-examples));
  current examples import via the **`reth-ethereum` meta-crate**
  (`reth_ethereum::exex::{…}`), not standalone `reth-exex`; 1.x → 2.x (revm 41.0.0,
  MSRV 1.95, edition 2024). Types are unchanged in shape:
  `ExExNotification::{ChainCommitted,ChainReorged,ChainReverted}`, `ExExContext`,
  `ExecutionOutcome { bundle: BundleState, .. }`;
  `BundleAccount.original_info.balance → info.balance` is the diff.
  Two gotchas: `ExExEvent::FinishedHeight` **must** be sent; and a `Chain` can span
  **multiple blocks** during backfill, so `bundle.state` is the *aggregate* — gate
  on `new.len() == 1` for strict per-block.
- **EIP-7928 Block-Level Access Lists are a 1:1 match** for this requirement —
  they record post-tx balances for senders/recipients, CALL value transfers,
  COINBASE, SELFDESTRUCT beneficiaries, *and* withdrawal recipients. Reth 2.4.0
  already ships `eth_getBlockAccessListByBlockNumber`. Gated on the **Amsterdam**
  fork (~Q3 2026) — not live on Sepolia today (public endpoints return `-32601`).
  When it lands, the whole feed becomes one RPC call. Worth a line in the paper.
- **No public address→balance dump exists.** What exists are client DB snapshots
  (PublicNode, snapshots.reth.rs), which still require running the client to read.

## 🔎 Novelty of the rewind (§6's open question): **not found in the literature**

The brief says a related-work check was running in parallel and to ask for its
result. Result: **not published**, across ~12 full texts (SimplePIR, DoublePIR,
FrodoPIR, ChalametPIR, AuthPIR, YPIR, HintlessPIR, Checklist, incpir, Plinko,
Lazzaretti, iSimplePIR). Every scheme that touches the problem does one of three
things — **patch the hint**, **eliminate the hint**, or **pin the server**. None
rewinds the response against a stale hint.

Two honest caveats: *Efficient Updatable PIR From Simulatable Homomorphic
Ciphertexts* (Tian & Lin, AsiaCCS 2025) is paywalled and could not be read (its
abstract points at "eliminate client hint storage", a different direction); and an
observation this simple may exist as unwritten folklore. Absence from the
literature is what is established.

**The closest prior work is iSimplePIR** (Lu, Xu, Li, Wang, Cui — ePrint
[2026/030](https://eprint.iacr.org/2026/030), Jan 2026 — *not* Hao et al. as the
brief's framing suggests). It is closer than the brief implies: it uses **the same
linearity identity** (`H′ = D′·A = D·A + (D′−D)·A`) and **already ships the client
the plaintext delta γ**. The entire difference is *where γ lands*:

| | iSimplePIR | this design |
|---|---|---|
| γ applied to | the hint | the response |
| cost per delta | `O(lwe_dim)` ≈ 1275 MACs | `O(1)` per delta per query |
| client hint must be current | **yes** — explicit, with versioned hint management and ordering checks | no |
| amortisation | better past ~`lwe_dim` queries | better below it |

So the contribution should be framed as **stateless / epoch-free**, not as
"nobody noticed the answer is linear". That is a narrower claim than the brief
implies but a real and defensible one — and it is exactly what §6 already says the
value is ("the value is schedulability").

**The strongest evidence is negative.** YPIR (Menon & Wu, USENIX Sec 2024) writes
down precisely this pain point and reaches for a heavyweight fix:

> "the approach in [HHC+23a] is to have clients download the hints on a weekly
> basis and **wait to test an SCT if its validity falls outside the time window
> associated with the current hint** … **The log server must also maintain
> multiple copies of the SCT database to support PIR queries for hints issued at
> different times.**"

Both problems dissolve under the rewind: one live copy serves every hint vintage,
and no client ever waits. If the trick were known, that is the paragraph it would
appear in. ChalametPIR calls dynamic updates "an interesting open problem";
HintlessPIR's own limitations state that on update "all hints need to be
recomputed"; AuthPIR states a changing DB "requires a client to obtain a fresh and
correct digest before making each PIR query".

**Required reading before any write-up**: the Ethereum Foundation's sharded-PIR
design notes (<https://notes.ethereum.org/U9xM4VOPR9isPK7lOZJUQg>, §5.2–5.3) hit
the identical wall — *"hints become stale and must be re-derived"* — and solve it
with a **sidecar**: pin the *engine* at a reference snapshot server-side and serve
changed keys from a small side database. Same effect, opposite lever: **they pin
the server, we pin the client.** This is the PSE "private reads" workstream the
brief's §1 invokes, and it is the design this observation would directly simplify.

---

## Environment constraints discovered

- **Cannot run a Sepolia node here**: macOS, 8 cores, 16 GB RAM, 303 GB free. The
  brief itself says the node wants `i4i`-class local NVMe or bare metal, and
  explicitly not the `r6a.4xlarge` in `docs/aws-bench-runbook.md`.
- `cast` and `anvil` are available; `geth`/`reth` are not.
- crates.io and GitHub are reachable (`axum 0.8.9`, `jsonrpsee 0.26.0`, `tokio
  1.52.3` resolve and build).
- Rust 1.85.0 (the repo's pin) is installed alongside 1.96.0.
- **`perf/optimized` exists only on the fork `bao-ninh-orochi/IKPIR`, not on
  `orochi-network/IKPIR`.** The git dep must target the fork and pin
  `rev = 042d868`, or the branch must be pushed to the org first. A branch name is
  not a reproducible dependency; pin the rev either way.
