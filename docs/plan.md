# Implementation plan — private `eth_getBalance` over RisePIR

Read [`verification.md`](verification.md) first: it is the evidence this plan rests
on, and it corrects the brief in five places, two of which change the design.

**The invariant that outranks everything: never return a wrong answer.** Erroring
is fine. Labelled-stale is fine. A silently wrong balance is total failure. Every
decision below is subordinate to this.

---

## 1. What changed versus the brief

Six findings reshape the plan. All are evidenced in `verification.md`.

0. **`prestateTracer` misses withdrawals — the brief's fix for its own "biggest
   trap" does not work.** Verified on Sepolia block `0xac2700`: the withdrawal
   recipient appears in **0 of 128** traces, and the missed amount matches the
   `eth_getBalance` delta exactly. `debug_traceBlock` and `trace_block` are both
   *tx-scoped*; EIP-4895 applies withdrawals *after* transactions, so there is no
   slot for them in the response. Only ExEx `BundleState` captures them natively.
   **Correct recipe: `prestateTracer(diffMode)` ⊕ `block.withdrawals[]` (gwei ×10⁹).**
0b. **Sepolia is not small.** A non-archive Reth Sepolia snapshot measures **735.7 GB
   compressed** (2026-07-15). The brief says "hours and modest disk", confidence
   High. It is in mainnet's league. **A node does not fit in 303 GB** — this forces
   the staged universe (ADR-0013), and it is a hardware limit, not a scheduling one.
0c. **Sepolia is ~150M accounts, not 1M–4M**, so it does *not* cross-validate the
   paper's benchmark band (that rationale for choosing Sepolia is void) — but it
   *does* something better: **it is already past the impossibility threshold**, so
   the §1 argument becomes measurable rather than extrapolated. See §2.
0d. **A public Sepolia balance dataset exists** (Xatu parquet, verified 12/12 against
   archive RPC) — §10 listed this as unknown. It replaces the ~100k-call conformance
   oracle with one 8 MB download. See Stage 1.

1. **No upstream changes are needed.** The brief budgeted two coordinated PRs. Both
   are avoidable — the backend trait family is fully `pub`, and the full rewind path
   (including the untested step 2–3) has been run end-to-end from outside the crate.
   *The project has no external coordination dependency.*
2. **The real upstream gap is a batch-mutation API**, which the brief did not
   anticipate. `IkpirServer` folds+patches+bumps per mutation, so a 300-change block
   costs 300 patches and 300 epochs. Batching measured **2.1–2.3×** faster and is
   semantically required for "epoch = block".
3. **Frodo is dead at Sepolia scale, not just mainnet** — the brief left this open
   and guessed Frodo might win. It cannot: the client needs `A` in full for every
   query, which is **19.7 GB** at 9.4M accounts. The question is closed: **RisePIR-S**.
4. **Per-block patch is not constant in wall-clock** (1.2 → 4.7 ms as N grows),
   because the entry-level patch strides a hint that outgrows cache. N-independent
   in op count, not in time. The headline survives with room to spare; the number
   must be reported honestly.

## 2. The headline, measured

The brief says the headline is the impossibility argument and the denominator is
full-rebuild time. Measured (SimplePIR, 3-ary, `bucket_size` 4, 12-byte balances,
~75% load, 300 updates/block, 8 cores, `perf/optimized` + `target-cpu=native`):

| accounts | full rebuild | per-block patch | ratio | rebuild duty cycle @12 s |
|---:|---:|---:|---:|---:|
| 36,864 | 10.8 ms | 1.175 ms | 9× | 0.09% |
| 589,824 | 138.2 ms | 3.213 ms | 43× | 1.15% |
| 2,359,296 | 652.1 ms | 4.293 ms | 152× | 5.43% |
| **9,437,184** | **4,912 ms** | **4.688 ms** | **1048×** | **41%** |

**Sepolia is ~150M accounts** (Correction 8 — the brief guessed 1M–4M). At 3-ary /
`bucket_size` 4 that is `num_buckets = 3·2²⁴` → 201M slots → **74.5% load exactly**,
`segment_rows = 2²⁴`. Extrapolating the measured curve:

| accounts | full rebuild | duty cycle @ 12 s block |
|---|---:|---:|
| 9.4M (measured) | 4.9 s | 41% — barely possible |
| **Sepolia ≈150M** | **79 s – 276 s** | **655% – 2302% → impossible** |
| mainnet ≈300M | 157 s – 757 s | 1310% – 6306% → impossible |

**This inverts §1's framing, in our favour: the impossibility argument no longer
needs a mainnet extrapolation.** It is demonstrable on a live public testnet today —
Sepolia is already 7–23× over the block budget, against a patch duty cycle of
**~0.08%**. Rebuild is memory-bandwidth-bound, so parallelism does not rescue it.
The possible→impossible crossover sits near ~20–30M accounts; Sepolia and mainnet
are both well past it. **Lead with this.** "The testnet already breaks classical
PIR" is stronger, and measurable, where "mainnet would break it" is projected.

⚠️ The brief's "10⁵–10⁶×" appears **~1 order optimistic**: it divides an
out-of-cache rebuild by an in-cache patch. Honest measured ratio is ~10³× at 9.4M,
extrapolating to ~10⁴–10⁵× at Sepolia/mainnet. Still decisive. Report ours.

⚠️ Consequence: **full Sepolia does not fit this box** — DB 12.88 GB + hints 0.50 GB
+ `A` 0.50 GB ≈ 13.9 GB before the account map, on 16 GB. Stage 2 needs ~32–64 GB
RAM *and* ~1 TB disk.

## 3. Architecture

### 3.1 The one interface (per §11)

```rust
pub struct BlockUpdate { pub block: u64, pub changes: Vec<(AddressHash, Balance)> }
```

Everything Ethereum lives behind this. Everything novel lives in front of it. This
is why Stage 0 needs no node.

### 3.2 Build on the primitives, not the wrappers — ADR-0001

We use the crate's **contributions** (`segmented-cuckoo`; `B::server_setup` /
`server_answer` / `server_patch_hint` / `client_setup` / `client_query` /
`client_decode` / `client_patch_state`) and reimplement the two thin orchestration
wrappers (`IkpirServer`, `IkpirClient`), whose *policy* does not fit this
application:

| Their policy | What we need |
|---|---|
| 1 epoch per mutation | 1 epoch per **block** |
| strict `q.epoch == server.epoch` | re-stamp (licensed by query/DB independence) |
| `apply_delta` only `epoch+1` | apply an arbitrarily coalesced delta |
| `decode` returns a value | we need **raw bucket cells** for the step-2 correction |
| no batch entry point | one drain, one fold, one patch per block |

Cost: ~150 lines (a 50-line re-fold + per-segment plumbing). Benefit: no upstream
dependency, correct semantics, one patch per block. We offer the **batch API**
upstream as the small PR the brief asked for — motivated by a measured problem.

### 3.3 The rewind is the mechanism — ADR-0003

Client holds `(A, H₀, block₀)` and a rolling public `ΔD`; it never has to be at the
server's head.

```
1. resp ← server.answer(q)            answered at the server's head E'
2. resp −= qᵀ·ΔD[block₀ → E']         → bit-exact the epoch-block₀ response
3. cells ← B::client_decode(state, resp)   → the bucket AS OF block₀
4. cells += ΔD[row]                   → the bucket AS OF E'    ← BEFORE step 5
5. scan cells for fp(key)
```

**Step 4 must precede step 5.** Verified in both directions: with a hint 10,500
epochs stale and a key inserted after the pin, scanning first returns `None` — i.e.
`0x0` for an account that exists. This is *the* silent-wrong-answer bug in this
design and it gets a dedicated regression test (`created_during_run`).

Hint patching demotes to **garbage collection of `ΔD`**, on any cadence, never
blocking a query. Two consequences drive the deployment:

- **(a) Steady-state query rate ≈ 0.** A key can only ever live in its `d` candidate
  buckets, which the client already tracks. So: query once per account, then follow
  the public delta stream forever, free and with zero leakage. PIR is for the *cold
  read* only. This dissolves the ~0.3–0.6 s answer latency concern.
- **(b) The client must download the full delta stream and filter locally.** Asking
  for "my buckets only" leaks exactly what PIR protects. This is why the wire format
  matters (§3.5).

### 3.4 Two calls, response names the epoch (brief §5, adopted unchanged)

```
answer(q)        → (responses, block = E')      # server answers at ITS head
sync(from_block) → (coalesced delta, to_block)  # identical bytes for every client
```

`sync` is CDN-cacheable precisely because it is client-independent; bundling the
delta into the response would make every blob bespoke. `answer` stays the pure hot
path.

### 3.5 Wire codec — ADR-0005

The crate's 10 B/cell (`u16` offset + `i64` delta) carries ~8 bits of payload.

**Key insight (new):** a coalesced delta telescopes — `Σ(c_{i+1} − c_i) = c_{E'} − c_E`
— and both endpoints are real cell states in `[0, p)`. So **|Δ| < p ≤ 2^14 no matter
how many blocks are coalesced.** The `i64` is ~4× wider than mathematically possible.

Encoding: `varint(offset_gap)` + `zigzag_varint(Δ)` ≈ **3 B/cell**. Offsets are
BTreeMap-sorted so gaps are small.

The bound doubles as a **free integrity check**: assert `0 ≤ cell + Δ < p` on apply.
A whole class of pipeline bugs then fails loudly instead of returning a plausible
wrong balance. It never fired across 13,220 cells / 500 blocks in testing.

⚠️ **Live trap:** measured compaction was only 1.66–1.93×, because synthetic small
integer balances change few cells. Realistic wei-scale balances change ~11–15 cells
per update. **The delta benchmark must use realistic balance data** or it will
understate both the naive size and the win.

### 3.6 Never-wrong enforcement points

| Hazard | Enforcement |
|---|---|
| balance ≥ 2^value_bits | **hard fail at ingest**, never truncate (ADR-0009) |
| coalesced delta out of range | assert `0 ≤ cell+Δ < p` in the codec |
| client behind / server stalled | label the block; report stalled; never guess |
| address outside a bounded universe | **error, not `0x0`** (Stage 1 only) |
| decode noise corrupts a *value* cell (fp still matches) | checksum inside the value → error, not a number (ADR-0009) |
| decode noise corrupts a *fp* cell | scan misses → `0x0`. Inherent; documented |
| cuckoo false positive (~2⁻²⁸) | inherent to the ChalametPIR line; documented; conformance includes nonexistent addresses |
| malformed/hostile bytes | **validate every length before allocating** |

### 3.7 Concurrency — ADR-0010

The brief says plan for one client instance per in-flight request. **Not needed**:
the concrete types are auto-`Send + Sync` (verified). `RwLock<Server>` gives
concurrent readers on the hot path; the writer takes it ~5 ms per 12 s block.
`Mutex<Client>` with **strict lockstep** honours the `&mut self` + FIFO contract
without cloning hundreds of MB per request — justified by consequence (a).

The epoch→block map must live **inside the same lock** as the server, so
`answer` + head-stamping are atomic w.r.t. the database.

## 4. Repo layout

```
crates/
  risepir-proto/    geometry calc, BlockUpdate, delta types, codec  (no I/O)
  risepir-server/   batched server over public primitives + delta ring + HTTP
  risepir-client/   rewind client + JSON-RPC :8545
  risepir-feed/     BlockUpdate producers: mock | rpc | (exex)
xtask/              conformance, bench, geometry
docs/adr/           one paragraph per decision (§11)
.cargo/config.toml  target-cpu=native   ← the §8 gotcha: git deps do NOT inherit it
```

Git dep: **fork + pinned rev**, since `perf/optimized` exists only on
`bao-ninh-orochi/IKPIR` and a branch name is not reproducible:

```toml
ikpir-common = { git = "https://github.com/bao-ninh-orochi/IKPIR", rev = "042d868" }
```

## 5. Stages

Per §11: *"Stage 0 needs no Ethereum at all… Do not start by syncing a node."*

### Stage 0 — the whole machine, synthetic (this is most of the engineering)

| # | Deliverable | Acceptance |
|---|---|---|
| 0.1 | `risepir-proto`: geometry calculator; codec | geometry reproduces the measured CSVs exactly; codec round-trips; `\|Δ\|<p` asserted; property tests |
| 0.2 | `risepir-server`: batched apply_block, delta ring, HTTP | 1 epoch + 1 delta bundle per block; malformed input → error, no panic/OOM |
| 0.3 | `risepir-client`: rewind path + JSON-RPC | steps 1–5 in order; `created_during_run` regression test |
| 0.4 | mock feed: 1M keys, ~300 changes/12 s | deterministic, seeded, **realistic wei-scale balances** |
| 0.5 | conformance vs. in-process ground truth | ≥1000 addrs × ≥100 blocks, all 5 categories, single pass/fail command |
| 0.6 | `cast balance --rpc-url http://localhost:8545` | returns the byte-identical value |

**Stage 0 exercises everything novel** — batch epochs, delta ring, coalescing,
epoch fast-forward, codec, rewind — and is fully verifiable without a node.

### Stage 1 — real Sepolia, bounded universe

**Primary feed: the Xatu `canonical_execution_balance_diffs` parquet dataset** —
this was found after the brief was written and it changes Stage 1 completely.

```
https://data.ethpandaops.io/xatu/sepolia/databases/default/
    canonical_execution_balance_diffs/1000/{block//1000*1000}.parquet
```

One chunk = **1000 consecutive blocks, ~218k rows, ~8.3 MB**, covering blocks
**1,066,000 → 10,073,999**. `to_value` is the **absolute post-tx balance**, so state
reconstructs in one pass with no replay. Verified 12/12 exact against live archive
RPC at block 5,000,000 (0 wei delta, reproduced on dRPC *and* Tenderly).

**A single 8 MB download replaces the entire ~100k-call conformance oracle** — no
node, no rate limits — provided the test window sits at blocks ≤10,073,999. Pick one
inside the covered range; do not try to reach the head (the 1.2M-block tail would
need `prestateTracer` on 1.2M blocks ⇒ days).

Three traps, **all confirmed by test, all silent-wrong-answer class**:
1. values are 32-byte **little-endian** uint256 — big-endian parses fine and yields ~1e76;
2. take the **LAST** diff per address per block by `(block, tx_index, internal_index)`
   — the first gives 911,999.122730 vs the correct 911,999.128165467 ETH;
3. **beacon withdrawal credits are absent** (every row is tx-bound). Same gap as the
   RPC path, from an independent source. Only 1–4 Sepolia withdrawal addresses, but
   they are among the **largest holders** — must be patched from RPC.

**Secondary/live feed: `https://sepolia.drpc.org`** — free, keyless, serves
`debug_traceBlockByNumber` + `prestateTracer` *and* archive `eth_getBalance`
(PublicNode and 1rpc reject archive heights; dRPC and Tenderly do not).

```
per_block_diff = prestateTracer(diffMode) over all txs     # value, internal CALLs, SELFDESTRUCT, gas, coinbase
               ⊕ eth_getBlockByNumber(...).withdrawals[]    # gwei ×10^9  ← WITHOUT THIS, SILENTLY WRONG
```
plus: **`post` is sparse** — `new_balance = post[a].balance ?? pre[a].balance`.
Absent means *unchanged*, **not zero**; an address in `pre` but wholly absent from
`post` is deleted.

A per-block `eth_getBalance` cross-check against the reference RPC is the guard: any
block where the derived balance disagrees fails the run loudly. **Both feeds must
agree with the oracle, including on a withdrawal block** — that is the test that
catches the trap the brief's own prescription walks into.

**Universe = accounts appearing in the chosen chunk window + a seed set** (~140
distinct addresses/block ⇒ ~14k over 100 blocks — small, and fully ground-truthed).

⚠️ **Honest gap, stated loudly:** with a bounded universe, "not in filter" means
"not in *my* universe", which is a superset of "does not exist". A real Sepolia
account outside the universe would get `0x0` — a wrong answer. Therefore Stage 1
**errors on not-found** rather than returning `0x0`, and its conformance sample is
drawn from (universe ∪ random-nonexistent). Random 20-byte addresses are
nonexistent with overwhelming probability, so the "never existed → `0x0`" category
is still tested honestly. **Only Stage 2 closes this**, because only full state
makes "not in filter ⟺ does not exist" true.

### Stage 2 — full Sepolia state (needs a node; code + runbook, cannot run here)

Blocked on **hardware** (735.7 GB), not on design. Enumerate at a fixed block `B`
under an MDBX snapshot-isolated read view, then replay `B+1..head`.

Three things the brief could not have known, which make this *easier* than it looks:

- **ExEx is the right tool and is current** (reth v2.4.0, 2026-07-14) — and it is
  the only prescription in §7 that actually captures withdrawals, for free. But it
  has moved: `examples/exex/` is gone from the tree (now
  `paradigmxyz/reth-exex-examples`), imports go via the `reth-ethereum` meta-crate,
  and `ExExEvent::FinishedHeight` must be sent. A `Chain` can span multiple blocks
  during backfill — gate on `new.len() == 1` for strict per-block.
- **Reth 2.x dropped `PlainAccountState` for hashed state**, which kills the classic
  enumeration recipe — *except for us*. We key by `keccak256(address)` anyway, so
  reth v2's `HashedAccountState` is our enumeration source **in our own key space**.
  The brief's keying decision (§7) pays off here in a way it did not anticipate.
  Note `debug_accountRange` on reth is a **no-op stub** (`Ok(())`) — do not use it.
- **EIP-7928 Block-Level Access Lists** collapse this whole feed into one RPC call
  (they record withdrawal recipients explicitly). Reth 2.4.0 ships it; gated on
  Amsterdam (~Q3 2026), not live on Sepolia. Worth a forward-looking line.

### Stage 3 — the numbers table (§0.5), measured not guessed

per-block patch time · per-block delta bytes · hint size · query/response bytes ·
answer latency · client memory · **full-rebuild time** (the §1 denominator) —
plus patch cost as a **curve over mutations/block**, since Sepolia's ~10–50
changes/block *understates* the mainnet operating point of ~300.

## 6. Decisions (full text in `docs/adr/`)

| ADR | Decision | Deviates from brief? |
|---|---|---|
| 0001 | Build on public primitives; reimplement the two wrappers | **Yes** — brief expected 2 upstream PRs |
| 0002 | RisePIR-S (SimplePIR) | **Yes** — brief left Sepolia open, guessed Frodo |
| 0003 | Rewind is the mechanism; hint patching is GC | No — brief §6 |
| 0004 | Epoch = block; batch mutations per block | No — brief §5 |
| 0005 | varint/zigzag codec; `\|Δ\|<p` as integrity check | Sharpens §5 (`i64`→`i16`-bounded) |
| 0006 | Raw HTTP + binary codec; deltas as immutable per-block objects + coalesced range | Brief left open |
| 0007 | Follow `finalized`; `"latest"` = our head, `eth_blockNumber` reports it | No — brief §7 |
| 0008 | Key = `keccak256(address)` | No — brief §7 |
| 0009 | value = `balance ‖ checksum`; hard-fail on overflow | **New** — closes a silent-wrong-answer path |
| 0010 | Strict lockstep behind a Mutex | **Yes** — brief said per-request instances |
| 0011 | Persist the authoritative account map; rebuild SCF+hint on restart | Brief left open |
| 0012 | Non-`getBalance` methods: **deny by default**, opt-in proxy with a loud warning | Brief leaned pragmatic-proxy |
| 0013 | Staged universe: mock → **complete mainnet snapshot** → optional node | **Revised** — mainnet, not bounded Sepolia |
| 0014 | Acquire snapshot from **BigQuery balances**; snap download as fallback | **New** — see `data-acquisition.md` |
| 0015 | Store **only nonzero** balances; `0x0` by absence | **New** — exact, halves the DB, closes the honesty gap |

Two open, deferred with reasons: **nonce in the value** (`eth_getTransactionCount`
nearly free, +50% `row_width` — measure first) and **shard parameter** (k-anonymity
dial; a good paper table, not pure PIR).

## 7. What cannot be done in this environment — and what that costs

- **No Sepolia node.** macOS, 8 cores, 16 GB RAM, 303 GB free; the brief itself
  says the node wants `i4i`-class NVMe or bare metal. ⇒ Stage 2 ships as code +
  runbook, not as a result. Stage 1 covers real chain data on a bounded universe.
- **The conformance oracle needs ~100k historical `eth_getBalance` calls**, i.e.
  archive access, because our `"latest"` is the finalized head and the reference
  RPC's is not. Must use batched JSON-RPC against an archive endpoint.
- Because `"latest"` = finalized head (~13 min stale), MetaMask will connect but
  show a lagging balance. Documented rather than papered over.

## 8. Risks, ranked

1. **Silent wrong answer via step-4/5 ordering.** Mitigation: regression test with
   accounts created during the run — the brief's own catch, now reproduced.
2. **Sepolia balances exceeding 2^96.** Testnet has no supply discipline. Mitigation:
   measure the true max before fixing the width; hard-fail regardless.
3. **prestateTracer missing withdrawals.** Withdrawals credit balances with *no
   transaction*; the brief calls tx-parsing "the single biggest trap". Mitigation:
   verify against a reference RPC per block, not once — a diff on any block fails
   the run.
4. **Delta bench on unrealistic data** understating both sizes (§3.5).
5. **Geometry chosen before the account count is known** → `TableFull` or waste.
   Mitigation: geometry is computed by a tool from the account count, never
   hardcoded; run at ~75% load for headroom.

## 9. Immediate next steps

1. **Target is mainnet, not Sepolia** (ADR-0013). Data acquisition is settled in
   [`data-acquisition.md`](data-acquisition.md): download the nonzero-balance snapshot
   from BigQuery `crypto_ethereum.balances` (or `goog_blockchain_ethereum_mainnet_us`),
   keep current from the block stream. Server fits in ~9 GB — it runs here.
2. **Decision gate (needs a GCP free-tier account; I can't run it):** one `bq` query to
   (a) confirm the table is fresh in 2026 and (b) get `count(*) WHERE eth_balance > 0`.
   That count fixes the geometry. Until then, Stage 0 proceeds on synthetic data.
3. Implement Stage 0 in order 0.1 → 0.6 (Sonnet). `risepir-proto` and `risepir-server`
   are done (72 tests); `risepir-client` (the rewind) is next — it was interrupted at
   the usage cap and needs re-running.
4. `cast balance` against the mock is the Stage-0 gate. Only then wire the real snapshot.
