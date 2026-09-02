# Plan — private `eth_getBalance` over RisePIR

Authoritative, current spec. Self-contained. Where this references evidence, it lives
in [`verification.md`](verification.md) (measured findings) and
[`data-acquisition.md`](data-acquisition.md) / [`sync.md`](sync.md).

**The invariant that outranks everything: never return a wrong answer.** Erroring is
fine. Labelled-stale is fine. A silently wrong balance is total failure. Every
decision below is subordinate to this.

---

## 1. What we are building

A proof-of-concept private Ethereum RPC:

- a **server** that follows Ethereum **mainnet**, holds a RisePIR database of every
  nonzero account balance, and advances one epoch per block;
- a **client** exposing standard Ethereum JSON-RPC on `localhost:8545` that answers
  `eth_getBalance` **without the server learning which account was asked about**;
- a **conformance run** proving the client's answers are byte-identical to a reference
  RPC across ≥1000 addresses × ≥100 consecutive blocks, including nonexistent, zero,
  contract, and created-during-run accounts.

The headline is not latency or hint size (those are inherited from SimplePIR). The
headline is the **impossibility argument**: classical PIR-with-preprocessing must
re-run setup on every state change; at Ethereum scale that is a ~minutes-long rebuild
every 12 s. RisePIR folds a block's ~300 changes into a **~5 ms** hint patch with no
dependence on database size. Measured full-rebuild vs. per-block patch is the number
that turns "faster" into "otherwise impossible" (§7).

## 2. Current state

Seven crates, all committed and pushed to `orochi-network/private-eth-getbalance`
(signed, Verified), **164 tests passing**, and the binary **runs against real
mainnet** (recorded live evidence in [`deploy.md`](deploy.md) §5):

| crate | what |
|---|---|
| `risepir-proto` | geometry calculator, `BlockUpdate`/`BlockDelta` (+withdrawal credits), value + delta codecs, keccak |
| `risepir-server` | batched per-block server; **verified fp ∧ `key_tag` store ops** (ADR-0017); credits (ADR-0018); delta ring; `from_parts` restart path |
| `risepir-client` | the response-rewind client |
| `risepir-feed` | `MockFeed` (seeded) + `snapshot` (BigQuery balances loader) + `rpc` (mainnet finalized follow) |
| `risepir-http` | axum transport (answer/sync/setup/head/delta) + binary wire codec + HTTP client |
| `risepir-rpc` | JSON-RPC `:8545` front end; `mock`/`mainnet` subcommands; state persistence; partial mode |
| `xtask` | conformance harness (Stage 0.5 gate) + `bench` numbers table (Stage 3) + CLI |

Beyond unit tests, three heavier gates: `cargo run -p xtask --release -- conformance`
(1201 addresses × 120 blocks, all five account categories, 0 mismatches);
`cargo test -p risepir-feed --release -- --ignored` (live: trace-derived balances
byte-exact vs an independent provider on a real finalized block); and the recorded
live deployment ([`deploy.md`](deploy.md) §5 — 8/8 private queries exact on real
mainnet blocks). `xtask bench` measures the Stage 3 numbers table into
[`docs/numbers.md`](numbers.md).

## 3. Architecture

### 3.1 The KV-SCF *is* the database — work directly in matrix D (ADR-0016)

There is **no** intermediate `keccak256(address) → balance` map. Each snapshot pair
`(address, balance)` is inserted directly into the Segmented Cuckoo KV store as
`fp(address) ‖ value`. That store's flat cell array **is** the RisePIR database
matrix `D` (`store.as_cells()`), which `server_setup` reads to build the hint. So the
store is the single source of truth; there is no "build a KV, then convert to D" step.
`RisePirServer` already works this way.

- **Recovery authority is the external snapshot source** (BigQuery / snap, §5), which
  is re-fetchable — so we do not need a local authoritative address→balance map. On
  `TableFull` or detected corruption we re-bootstrap from it.
- The store is keyed by `keccak256(address)` (ADR-0008): free (the SCF hashes the key
  anyway), the client computes it itself, and it matches reth's hashed state, so a
  hashed account dump drops straight in with no address preimages.

### 3.2 Store only nonzero balances; `0x0` by absence (ADR-0015)

`eth_getBalance` returns `0x0` for a nonexistent account **and** for a zero-balance
one — indistinguishable. So the database holds only **nonzero** balances; a lookup
that misses the filter returns `0x0`, which is exactly correct. This (a) shrinks the
set from ~300M ever-seen to ~100M nonzero, and (b) makes "absent ⟺ zero" an *exact*
equivalence, so there is no bounded-universe honesty gap. A balance going to zero is a
**delete**; zero→nonzero is an **insert**; nonzero→nonzero is an **update**.

Revisited at the user's request and **kept** (ADR-0017): the unease about deletes was
real but belonged to the store's fp-only first-match ops, not to nonzero-only storage
— every key-addressed op now goes through a verified fp ∧ `key_tag` candidate scan
(`risepir-server::verified`), absent-key deletes are provable no-ops, and ambiguous
probe states fail loudly instead of corrupting a colliding account. Store-all would
have tripled RAM without removing that hazard class. EIP-4895 withdrawal credits ride
`BlockUpdate::credits` and resolve against the verified stored prior inside
`apply_block` (ADR-0018).

### 3.3 The rewind is the mechanism; hint patching is garbage collection (ADR-0003)

The client pins `(A, H₀, block₀)` plus a rolling public `ΔD` and **never has to be at
the server's head**. Query flow:

```
1. resp  ← server.answer(q)              // answered at the server's head E'
2. resp -= qᵀ·ΔD[block₀ → E']            // → BIT-EXACT the response a block₀ server gives
3. cells ← client_decode(state, resp)    // → the bucket AS OF block₀ (decodes vs the STALE hint)
4. cells += ΔD[row]                      // → the bucket AS OF E'    *** BEFORE STEP 5 ***
5. scan cells for the key                // fp → key-tag → checksum
```

**Step 4 must precede step 5.** A key inserted or kicked after `block₀` has no
matching fingerprint in the *pinned* bucket, so scanning first returns `None` — i.e.
`0x0` for an account that exists. This is *the* silent-wrong-answer bug; it is pinned
by `ordering_trap_is_real` in both directions.

Consequences that shape the deployment:
- **Steady-state query rate ≈ 0.** A key can only ever live in its `d` candidate
  buckets, which the client already tracks — so query an account **once**, then follow
  the public delta stream forever, free and with zero leakage. PIR is for the cold
  read only.
- **The client downloads the full delta stream and filters locally.** Asking for "my
  buckets only" would leak exactly what PIR protects.

### 3.4 Two calls; the response names the epoch (ADR-0006)

```
answer(q)        → (responses, block E')     # server answers at ITS head
sync(from_block) → (coalesced delta, E')     # identical bytes for every client → CDN-cacheable
```

Epoch = block (ADR-0004): all of a block's changes fold into **one** hint patch and
**one** delta bundle (not ~300, as the upstream one-epoch-per-mutation path would).

### 3.5 The value field (ADR-0009) — a 64-bit-effective fingerprint + integrity

The SCF keeps its **32-bit** positioning fingerprint (`fingerprint_bits = 32`,
unchanged upstream). The **value** carries two tags around the balance:

```
value = key_tag(32b) ‖ balance(96b) ‖ checksum(16b)          // widths tunable
```

- `key_tag = H₂(address)` extends the effective key fingerprint to **64 bits**
  → false-positive rate ~**2⁻⁶⁰** (vs ~2⁻²⁸ at 32-bit). This delivers the "64-bit
  fingerprint" decision **with no upstream change** and at **identical cost** to a
  true 64-bit SCF fingerprint (same total slot bits, so same `cells_per_slot`). See
  ADR-0009 for why this beats changing the primitive's fingerprint type to `u64`.
- `checksum = H(balance)` catches LWE decode corruption of the balance cells.

Client scan order per candidate slot: SCF-fp match → `key_tag` match (*is this our
key?* — mismatch ⇒ keep scanning ⇒ `NotFound` ⇒ `0x0`) → `checksum` match (*did the
balance decode cleanly?* — mismatch ⇒ `DecodeFailed` ⇒ **error, never `0x0`**). This
distinction is what keeps a corrupted read from silently becoming a wrong balance.

`H₂`/`H` are any fast hash (e.g. xxh3 with distinct seeds). Widths are tunable knobs
in the geometry tool; defaults above give 64-bit FP resistance and 2⁻¹⁶ corruption
miss.

### 3.6 Concurrency (ADR-0010)

The concrete types are auto-`Send + Sync` (verified), so `RwLock<Server>` gives
concurrent readers on the hot path and the writer takes it ~5 ms per block.
`Mutex<Client>` with strict lockstep honours the `&mut self` + FIFO query contract
without per-request client instances (each would carry ~500 MB). The epoch→block map
lives inside the server lock, so `answer` + head-stamping are atomic w.r.t. the DB.

## 4. Repo layout & the value-encoding upgrade

```
crates/
  risepir-proto/    geometry, BlockUpdate/BlockDelta, slot(value) codec, delta codec   [built]
  risepir-server/   batched per-block server over public primitives + delta ring       [built]
  risepir-client/   response-rewind client                                              [built]
  risepir-feed/     BlockUpdate producers: mock | rpc | (exex)                          [mock built]
  risepir-http/     axum transport + binary wire codec + HTTP client                    [built]
  risepir-rpc/      JSON-RPC :8545 front end + demo binary (private eth_getBalance)      [built]
  xtask/            conformance harness + CLI (the Stage 0.5 gate)                       [conformance built]
.cargo/config.toml  target-cpu=native   ← git deps do NOT inherit the upstream perf config
```

Git dep on the PIR primitive (pin the tag — the branch lives only on the fork):
```toml
ikpir-common = { git = "https://github.com/bao-ninh-orochi/IKPIR", tag = "v0.2.0-perf" }   # perf/optimized tip
```
(Currently path deps to a local checkout; switch to the git dep before hand-off is
final.)

### 4.2 The value-encoding upgrade **[DONE — ADR-0009]**

Implemented (§3.5). `ValueCodec` is now a slot codec: `encode(address_hash, balance) →
key_tag ‖ balance ‖ checksum` bytes, and `decode(address_hash, value_bytes) → Lookup`
(`Found/NotFound/DecodeFailed`, owned by `risepir-proto`). `key_tag = xxh3_64_with_seed(
address_hash, SEED≠0)` — a *different* seed from the SCF's own seed-0 fingerprint, so the
two 32-bit tags are independent and combine into a 64-bit-effective fingerprint (the SCF
fingerprint stays 32 bits; no upstream change). `Geometry::for_accounts` threads the
widths by taking the `ValueCodec` and deriving `value_bits` from it; `value_bits` itself
stays the opaque sizing scalar.

The one non-obvious correctness point: `risepir-client`'s scan masks each candidate slot
on **`fp` AND `key_tag` jointly** (a single constant-time `ct_eq(fp) & ct_eq(key_tag)`
mask) before the OR-select — *not* fp-only-then-check-tag. Selecting on fp alone would
OR a present account's value together with a different key's fp-colliding value into
garbage and wrongly report `NotFound` (`0x0`) for an existing account — a 2⁻²⁸
silent-wrong-answer. Joint masking makes the colliding slot contribute nothing, so the
combined resistance is a true ~2⁻⁶⁰.

## 5. Data: snapshot then follow (ADR-0013/0014, details in `data-acquisition.md` + `sync.md`)

- **Initial snapshot** (the only hard part): download nonzero balances from **BigQuery
  `crypto_ethereum.balances`** (`WHERE eth_balance > 0`, free tier, export to GCS), or
  `goog_blockchain_ethereum_mainnet_us` if it is fresher, or an **account-only snap
  download** (Nethereum `SnapSyncClient`) as the trustless fallback. Stream pairs
  straight into the KV-SCF (§3.1).
- **Bootstrap seam**: the snapshot is at some block `B_snap`; replay `B_snap+1..head`
  through the feed to catch up (bulk via Xatu diffs for the gap, live RPC for the
  tail), then steady-state follow. Reconcile at the join against archive `getBalance`.
- **Follow**: one `BlockUpdate` per block from the feed → `apply_block` → push the
  delta to the ring. Full loop, cadence, withdrawal handling, and reconciliation in
  [`sync.md`](sync.md).
- **Decision gate before the complete-set run** (needs a GCP account; cannot run from
  the dev box): one `bq` query confirms freshness *and* returns
  `count(*) WHERE eth_balance > 0` (fixes the geometry) *and* the snapshot block —
  scripted verbatim in [`deploy.md`](deploy.md) §2.1, alongside the export commands
  and the run itself. Everything up to that gate is built and live-verified
  ([`deploy.md`](deploy.md) §5).

## 6. Stages

Per the brief: Stage 0 needs no chain. Get `cast balance` working against the mock
before touching real data.

| # | Deliverable | Gate |
|---|---|---|
| 0.1 ✅ | value-encoding upgrade (§4.2) | **done** — 85 tests green; FP rate ~2⁻⁶⁰ asserted |
| 0.2 ✅ | `risepir-feed` mock: 1M keys, ~300 changes/12 s, **realistic wei-scale balances** | **done** — deterministic/seeded; delete-on-zero + end-to-end pipeline test |
| 0.3 ✅ | HTTP transport (answer/sync/setup/head) + delta objects | **done** — exact-length codec, fuzzed no-panic/OOM, end-to-end over HTTP |
| 0.4 ✅ | JSON-RPC `:8545` (`eth_getBalance`, `eth_chainId`, `eth_blockNumber`, `net_version`) | **done** — `cast balance --rpc-url localhost:8545` verified against the mock |
| 0.5 ✅ | conformance vs. in-process ground truth | **done** — `xtask conformance`: 1201×120, all 5 categories, 0 mismatches, exit 0 |
| 1.a ✅ | snapshot ingest (BigQuery balances CSV/CSV.gz → KV-SCF) | **done** — strict parser, geometry from real count ([`deploy.md`](deploy.md) §2) |
| 1.b ✅ | `risepir-feed` rpc: finalized follow, prestateTracer ⊕ withdrawals | **done** — live gate: trace-derived balances byte-exact vs an independent provider |
| 1.c ✅ | mainnet binary: bootstrap/follow/reconcile/persist + partial mode | **done** — live: 8/8 private queries exact on real blocks ([`deploy.md`](deploy.md) §5) |
| 1.d | complete-set run: user-run `bq` gate + snapshot export + 16–24 GB box | the one remaining step — runbook [`deploy.md`](deploy.md) §2, gate query included |
| 3.x ✅ | numbers table (measured, not guessed) | **done** — `xtask bench` → [`docs/numbers.md`](numbers.md); full-rebuild denominator measured |

## 7. The headline, measured

SimplePIR, 3-ary, 12-byte balances, ~75% load, 8 cores, `perf/optimized` +
`target-cpu=native`:

| accounts | full rebuild | per-block patch | duty cycle @12 s |
|---:|---:|---:|---:|
| 9.4M (measured) | 4.9 s | ~4.7 ms | 41% |
| ~100M (mainnet nonzero, extrapolated) | ~52–150 s | ~5–10 ms | 430–1250% → **impossible** |
| ~300M (all accounts) | ~157–757 s | ~5–10 ms | 1300–6300% → **impossible** |

Rebuild is memory-bandwidth-bound, so parallelism does not rescue it. Report ours (the
brief's "10⁵–10⁶×" divides an out-of-cache rebuild by an in-cache patch; the honest
measured ratio is ~10³× at 9.4M, ~10⁴–10⁵× at mainnet). Server RAM at ~100M nonzero
with the §3.5 encoding: **~12 GB** — it runs on a normal machine.

The full measured Stage-3 table — every scale, the per-block-patch curve over
mutations/block, delta compaction on realistic balances, sizes, and answer latency — is
in [`docs/numbers.md`](numbers.md), produced by `xtask bench`. It is measured against the
workspace's then-pinned IKPIR `perf/optimized` commit (`0f3b99b`, tagged `v0.1.0-perf`;
the workspace has since moved to `v0.2.0-perf`, ADR-0046); see that file's "IKPIR build"
note — the pin and the numbers move together, never separately.

## 8. Never-return-a-wrong-answer checklist

| hazard | enforcement |
|---|---|
| balance ≥ 2^balance_bits | hard-fail at ingest, never truncate (ADR-0009) |
| fp-only store op hits a colliding foreign entry | verified fp ∧ `key_tag` scan before every update/delete; absent-delete = no-op; shadowed/duplicate → loud `FingerprintAmbiguity` (ADR-0017) |
| cuckoo false positive | 64-bit effective fingerprint → ~2⁻⁶⁰ (§3.5); conformance includes nonexistent addrs |
| decode corrupts a balance cell | value checksum → `DecodeFailed` (error), never a number |
| step-4/5 ordering | `ordering_trap_is_real`; conformance includes created-during-run accounts |
| feed misses withdrawals | `prestateTracer ⊕ block.withdrawals[]`; per-block reconcile vs archive RPC (`sync.md`) |
| client behind / server stalled | label the block; report stalled; resync outside the ring window; never guess |
| coalesced delta out of range | assert `0 ≤ cell+Δ < p` in the codec (free integrity check) |
| malformed/hostile bytes | validate every length before allocating |

## 9. Decisions

Full text with rationale and rejected alternatives in [`adr/README.md`](adr/README.md).
Load-bearing ones: RisePIR-S over Frodo (0002); rewind-as-mechanism (0003); epoch=block
+ batch (0004); 64-bit-effective value encoding (0009); KV-SCF-is-the-database (0016);
store-nonzero-only (0015); snapshot-then-follow from BigQuery (0013/0014); follow
`finalized` (0007); deny-by-default RPC surface (0012).
