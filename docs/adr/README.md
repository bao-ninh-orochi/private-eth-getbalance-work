# ADR log

One paragraph per decision: what was chosen, what was rejected, why. Per the
brief's §11 — *"That log is the thing that makes this handoff-able and is worth as
much as the code."*

The brief's §10 decisions are **strong priors, not orders**: *"If you find a better
path, take it — but say so explicitly and say why. Silent deviation is the failure
mode; reasoned deviation is what you are for."* Deviations are marked **[DEVIATES]**
and each carries its evidence.

---

### ADR-0001 — Build on the public primitives; reimplement the two wrappers **[DEVIATES from §8]**

**Chosen:** drive `segmented-cuckoo` and the `IndexPirBackend` /
`IncrementalPirBackend` trait methods directly, and reimplement the thin
`IkpirServer` / `IkpirClient` orchestration layers in our crates.
**Rejected:** consuming `IkpirServer`/`IkpirClient` and landing two upstream PRs, as
§8 prescribed.
**Why:** §8 says two changes "genuinely need upstream changes" — a multi-epoch
`apply_delta` and an in-crate `client_decode` for the rewind. Both are avoidable:
the trait family and all wire fields are `pub`, and I ran the **entire** rewind path
(including the untested step 2–3) from outside the crate, plus a 500-block coalesced
`client_patch_state`, with zero upstream changes. Meanwhile the wrappers' *policy*
actively fights this application: one epoch per mutation, strict `q.epoch ==
server.epoch`, `apply_delta` only at `epoch+1`, and `decode` returns a value where
we need raw bucket cells. Cost ≈150 lines; benefit: no external coordination
dependency at all, and correct semantics.
**Consequence:** the *real* upstream gap is a **batch-mutation API** (ADR-0004),
which we offer as the small PR §8 asked for — motivated by a measured problem
rather than an anticipated one.

### ADR-0002 — RisePIR-S (SimplePIR) **[DEVIATES from §4/§10]**

**Chosen:** SimplePIR. **Rejected:** FrodoPIR.
**Why:** §10 rates "SimplePIR at mainnet" High but "low for Sepolia — measure both",
and §4 says *"at Sepolia scale Frodo may well win — a 1.2 MB constant hint is
extremely attractive for a wallet."* It cannot win: the Frodo **client** needs `A =
n_rows × lwe_dim × 4` **in full for every query** (`b = A·s + e + Δ·u_row` touches
every row, so it cannot be sampled lazily) — **19.7 GB at 9.4M accounts, 315 GB at
Sepolia's real 150M**. The 1.2 MB hint is irrelevant; the client is dead on `A`
before the hint matters. Frodo's rebuild is also ~5× slower at equal N (measured:
756 ms vs 138 ms at `seg_rows = 65536`). SimplePIR at Sepolia scale: hint 501 MB, `A`
501 MB, query/response 393 KB.
**Kept:** Frodo stays in the numbers table as the measured justification, so the
claim is evidenced rather than asserted.

### ADR-0003 — The rewind is the mechanism; hint patching is garbage collection

**Chosen:** clients pin `(A, H₀, block₀)` and correct responses by `qᵀ·ΔD`.
**Rejected:** keeping clients synchronised to the server's head via `apply_delta`.
**Why:** §6, adopted. The value is **schedulability**, not the FLOP saving: the
client never has to be at the server's epoch, which deletes the
`StaleEpoch`/`FutureDelta`/resync state machine from the hot path and makes an
offline client a non-event. Verified: a hint 10,500 epochs stale still answers
correctly, including for accounts created after the pin.
**Related-work check (§6 asked for it):** **not found** in ~12 full texts. Closest is
**iSimplePIR** (ePrint 2026/030, Jan 2026 — *not* Hao et al.), which uses the *same*
linearity identity and *already ships the client the plaintext delta*, but applies it
to the **hint**, and explicitly requires the hint to be current (with versioned hint
management). The whole difference is *where γ lands*: hint (`O(lwe_dim)`/delta +
version sync) vs response (`O(1)`/delta/query, stateless). **Frame the contribution
as stateless/epoch-free**, not as "nobody noticed the answer is linear."

### ADR-0004 — Epoch = block; batch all of a block's mutations into one patch

**Chosen:** our epoch **is** the block number; one drain, one fold, one hint patch,
one delta bundle per block. **Rejected:** the crate's one-epoch-per-mutation model.
**Why:** §5 wants one epoch per block; the crate gives ~300. Measured 2.1–2.3×
faster batched, but the real reason is semantic: a block is the natural atomic unit
and produces exactly one cacheable delta object. A `batched_equals_per_mutation`
test pins that batching changes **cost, not semantics** (bit-identical hints).

### ADR-0005 — Compact wire codec; the `|Δ| < p` bound doubles as an integrity check

**Chosen:** `varint(offset_gap)` + `zigzag_varint(Δ)` ≈ 3 B/cell.
**Rejected:** the upstream 10 B/cell (`u16` + `i64`); also rejected the brief's
suggested `i32`, as still too wide.
**Why:** §5 is right that this matters (every client, every block) but understates
it. **A coalesced delta telescopes to `c_final − c_initial`, and both endpoints are
real cell states in `[0, p)` — so `|Δ| < p ≤ 2^14` no matter how many blocks are
coalesced.** Coalescing is free in delta magnitude and the `i64` is ~4× wider than
mathematically possible. The bound is also a **free integrity check**: assert
`0 ≤ cell + Δ < p` on apply and a whole class of pipeline bugs fails loudly instead
of returning a plausible wrong balance. It never fired across 13,220 cells / 500
blocks.
**Trap:** measured compaction was only 1.66–1.93× because synthetic small-integer
balances change few cells; realistic wei-scale balances change ~11–15 cells/update.
**The delta benchmark must use realistic balance data.**

### ADR-0006 — Raw HTTP + binary codec; deltas as immutable per-block objects

**Chosen:** `POST /answer` (binary), `GET /delta/{block}` (immutable, cacheable
forever), `GET /sync?from=&to=` (coalesced), `GET /setup`, `GET /head`.
**Rejected:** JSON-RPC between client and PIR server (base64 inflates ~400 KB
payloads ~1.37×); gRPC (protobuf codegen for what is a length-prefixed `Vec<u32>`).
**Why:** §5's cacheability argument is the whole point — the delta from E to E′ is
identical for every client, so per-block objects are CDN/gossip-friendly and
`sync` is a convenience. §8 notes upstream has no serde *by design*
(`wire.rs:13-18`: a deployment should layer its own); this is that layer.

### ADR-0007 — Follow `finalized`; `"latest"` means our head

**Chosen:** follow `finalized`; `eth_blockNumber` reports our head; `"latest"` = that
head. **Rejected:** following `latest` (reorg bug class), `safe` (middle option).
**Why:** §7. Costs ~13 min staleness, deletes an entire bug class, and is
spec-compliant. **Consequence to document loudly:** our `"latest"` ≠ a public RPC's
`"latest"`, so conformance must compare at an explicit block number (archive query),
and MetaMask will connect but show a lagging balance. Reorg-by-negated-delta is
elegant (deltas are additive) but deferred — §7 says implement only if time allows.

### ADR-0008 — Key = `keccak256(address)`

**Chosen:** per §7. **Why:** free (the SCF hashes the key anyway), the client
computes it itself, and it balances any future sharding.
**Unplanned payoff:** reth 2.x **dropped `PlainAccountState` for hashed state**,
which kills the classic "walk plain state" enumeration recipe — *except for us*,
because `HashedAccountState` is already in our key space. A decision the brief made
for other reasons turns out to solve a problem it didn't know existed.

### ADR-0009 — `value = balance ‖ checksum`; hard-fail on overflow **[NEW]**

**Chosen:** carry a checksum inside the value; **error** on mismatch; **hard-fail**
at ingest if `balance ≥ 2^balance_bits`. Checksum width is configurable (0 = off) so
its cost is measurable.
**Rejected:** bare 12-byte balances, which the brief's §3 proposes.
**Why:** two silent-wrong-answer paths the brief does not close. (1) §3's "12 bytes
holds any balance" rests on mainnet's ~1.2e26 wei supply; **Sepolia is a testnet with
faucet-minted ETH and no supply discipline**, so the premise may not hold — silent
truncation is exactly what the invariant forbids. (2) The fingerprint authenticates
the *key*; **nothing authenticates the value**. If LWE noise corrupts a value cell
while the fingerprint still matches, the RPC returns a **corrupted balance**. A
checksum converts that into a loud error. `value_bits` is ours to choose, so this
costs ~+12% `row_width` at 16 bits/`pb=8` and needs no upstream change.
**Note:** if noise corrupts a *fingerprint* cell the scan misses and we answer
`0x0` — inherent, documented, not fixable in our layer.

### ADR-0010 — Strict lockstep behind a `Mutex` **[DEVIATES from §8]**

**Chosen:** one shared `Mutex<Client>`, queries strictly serialised.
**Rejected:** one client instance per in-flight request, which §8 prescribes.
**Why:** §8 says the types "carry no `Send`/`Sync` bounds" and concludes per-request
instances are needed. True at the *trait* level, but the **concrete types are
auto-`Send + Sync`** — verified by compiling `assert_send`/`assert_sync` for both
backends, both servers, and all four bundles. The real constraint is only `&mut
self` + FIFO consumption, which a mutex satisfies. Per-request instances would each
carry ~500 MB of `A`+hint at Sepolia scale — untenable. Justified by §6's
consequence (a): the steady-state query rate is ≈0, because a client queries an
account **once** and then tracks it from the public delta stream forever.

### ADR-0011 — Persist the account map; rebuild SCF + hint on restart

**Chosen:** persist our authoritative `address_hash → balance` map + the block
number; rebuild SCF and hints on start. **Rejected:** persisting SCF cells/hints.
**Why:** §9 requires deciding this. `CuckooKVStore` **discards keys** after hashing,
so IKPIR cannot rebuild itself from internal state — the app layer must hold the
authoritative map anyway for `TableFull` recovery (`server.rs` documents exactly
this). Given that, persisting the map is strictly simpler, and restart cost = the
full-rebuild time we already measure as the §1 denominator. Honest and self-consistent.

### ADR-0012 — Non-`eth_getBalance` methods: deny by default, opt-in proxy **[DEVIATES from §10]**

**Chosen:** answer `eth_getBalance` privately; answer `eth_chainId`, `net_version`,
`eth_blockNumber` locally (no leak); **deny everything else** unless
`--proxy-upstream <url>` is passed, which prints a loud startup warning.
**Rejected:** proxying by default, which §10 calls "pragmatic and probably right".
**Why:** the default should be private-by-default; proxying leaks exactly the calls
PIR exists to hide, and a privacy tool whose default configuration leaks is
mis-designed. Opt-in keeps the wallet path available while making the trade explicit
at the moment it is taken. `cast balance` needs only `eth_getBalance`(+`eth_chainId`)
and works with the default.
**Deferred:** adding **nonce** to the value would make `eth_getTransactionCount`
nearly free and double the private surface, at ~+50% `row_width` — measure first.

### ADR-0013 — Staged universe: mock → bounded Sepolia → full state **[NEW, forced by hardware]**

**Chosen:** Stage 0 synthetic (complete universe, full machine); Stage 1 real Sepolia
over a **bounded** universe from the Xatu dataset; Stage 2 full state, shipped as
code + runbook.
**Rejected:** going straight at full Sepolia state, which §0 implies.
**Why:** a Sepolia node needs **735.7 GB** (measured) against 303 GB free, and a
full-Sepolia server needs **~13.9 GB RAM** against a 16 GB box. Hardware, not
scheduling.
**The honest gap, stated plainly:** with a bounded universe, "not in filter" means
"not in *my* universe" ⊋ "does not exist", so a real account outside it would get
`0x0` — a wrong answer. Therefore **Stage 1 errors on not-found rather than
returning `0x0`**, and draws its conformance sample from (universe ∪
random-nonexistent); random 20-byte addresses are nonexistent with overwhelming
probability, so §0's "never existed → `0x0`" category is still tested honestly.
**Only Stage 2 closes this**, because only full state makes "not in filter ⟺ does
not exist" true.
