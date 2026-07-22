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

### ADR-0009 — Value = `key_tag ‖ balance ‖ checksum`; 64-bit-effective fingerprint; hard-fail on overflow **[REVISED — folds in the 64-bit-fingerprint decision]**

**Chosen:** the SCF keeps its 32-bit positioning fingerprint; the **value** carries
`key_tag(32b) ‖ balance(96b) ‖ checksum(16b)` (widths tunable). `key_tag = H₂(address)`
extends the effective key fingerprint to **64 bits**; `checksum = H(balance)` guards
the balance. Ingest **hard-fails** if `balance ≥ 2^balance_bits` (never truncates).
**Rejected:**
- bare 12-byte balances (silent-corruption and 2⁻²⁸ false-positive risk);
- changing `segmented-cuckoo`'s fingerprint type from `u32` to `u64` to get a literal
  64-bit fingerprint — an upstream change to the primitive, and **unnecessary**: the
  fingerprint's only job is disambiguation, and a 32-bit SCF fingerprint plus a 32-bit
  `key_tag` in the value requires 64-bit agreement to accept a slot, giving **identical
  ~2⁻⁶⁰ false-positive resistance at identical size** (total slot bits, hence
  `cells_per_slot`, are the same either way). Positioning with 32 bits is ample.
**Why:** three silent-wrong-answer paths, all closed in our own layer (`value_bits` is
ours to choose, zero upstream change):
1. **False positives.** A query for a nonexistent/zero account can hit a slot whose
   fingerprint collides. At 32-bit that is ~2⁻²⁸ — a wrong answer (garbage instead of
   `0x0`). The 64-bit-effective tag drops it to ~2⁻⁶⁰. *(This is the user's decision:
   "use a 64-bit fingerprint to mitigate the false-positive rate.")*
2. **Balance corruption.** If LWE noise corrupts a value cell while the key still
   matches, the RPC would return a corrupted balance. The checksum turns that into
   `DecodeFailed` (error), never a number.
3. **Overflow.** Silent truncation of a balance ≥ `2^balance_bits` is exactly what the
   invariant forbids; hard-fail instead.
**Scan order (client):** SCF-fp → `key_tag` (mismatch ⇒ not our key ⇒ keep scanning ⇒
`NotFound` ⇒ `0x0`) → `checksum` (mismatch ⇒ `DecodeFailed`). Keeping `key_tag` and
`checksum` separate is what preserves the `NotFound`-vs-`DecodeFailed` distinction — a
single combined tag `H(address‖balance)` would make a corrupted real account look like
"not found" and answer `0x0` (a wrong answer).
**Cost:** value grows to 144 bits ⇒ `row_width` ~88 at `bucket_size 4`/`pb 8` (~1.4×
the untagged DB). Tunable: narrower tags trade FP/corruption resistance for size.
**Residual:** if noise corrupts an SCF-*fingerprint* cell the scan misses and we answer
`0x0` — inherent to the filter layer, documented, not fixable here.

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

### ADR-0011 — Persist the KV-SCF (matrix D) + hints; snapshot source is the recovery authority **[REVISED — no separate account map]**

**Chosen:** the KV-SCF cell array (matrix `D`) and the per-segment hints **are** the
persisted database; serialize and reload them on restart. Do **not** keep a separate
authoritative `address → balance` map. On `TableFull` or detected corruption,
re-bootstrap from the external snapshot source (BigQuery / snap, ADR-0014), which is
re-fetchable.
**Rejected:** holding a local `HashMap<address_hash, balance>` alongside the store
(the earlier plan). Superseded by ADR-0016 — the user's decision to work directly in
`D`.
**Why:** the store *is* the database (ADR-0016); persisting it is the natural, minimal
choice and avoids a second copy that could drift from `D`. `CuckooKVStore` discards
addresses after hashing, so it cannot rebuild itself at a *larger* `num_buckets` — but
that is only needed on `TableFull`, and the snapshot source already gives an
authoritative, re-fetchable rebuild input, so no local map is required. At ~75% load
`TableFull` is years away regardless.
**Reading a current balance by address** (e.g. to apply a withdrawal to its prior
value, `sync.md`) goes through the store's key lookup — reliable at the 64-bit-effective
fingerprint of ADR-0009 — or falls back to `eth_getBalance` for the ~32k withdrawal
addresses. Restart cost when reloading persisted `D`+hints is I/O, not a full setup.

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

### ADR-0013 — Staged universe: mock → complete mainnet snapshot **[REVISED — supersedes the bounded-Sepolia plan]**

**Chosen (revised):** Stage 0 synthetic (complete universe, full machine); Stage 1
**complete mainnet** nonzero-balance set from a downloadable snapshot (ADR-0014),
kept current from the block stream; Stage 2 (self-hosted node) demoted to optional.
**Superseded:** the earlier "bounded Sepolia universe" plan, which existed only to
work around not having full state.
**Why the change:** two findings dissolved the constraint that forced bounding.
(1) ADR-0015 — storing only *nonzero* balances plus (2) ADR-0014 — a downloadable
mainnet balance snapshot together make a **complete** account set acquirable in
single-digit GB, with no node. Once the set is complete, "not in filter ⟺
zero-or-nonexistent ⟺ answer `0x0`" is **exact**, so the honesty gap that the bounded
universe created — where not-found had to error because it might be a real account we
lacked — simply disappears. Stage 1 returns `0x0` for not-found, matching real RPC
semantics, and the §0 "never existed → `0x0`" category is genuinely satisfied rather
than approximated.
**Mainnet over Sepolia** (also the user's stated preference): Sepolia turned out to be
~150M accounts / 735 GB / announced sunset (Corrections 7–8), so it buys none of the
smallness it was chosen for, and its 1M–4M cross-validation rationale was already void.
Mainnet is now the *easier* target, not the harder one.
**Residual honesty note:** completeness is only as good as the snapshot. A snapshot
missing accounts (stale dataset, incomplete snap walk) reintroduces the wrong-answer
risk — so ingest validates the snapshot's account count against an independent estimate
and the conformance run diffs against a live archive RPC across the sample. Never trust
the snapshot blindly.

### ADR-0014 — Acquire the snapshot from BigQuery balances; snap download as fallback **[NEW]**

**Chosen:** obtain the initial `address → balance` snapshot from the public **BigQuery
`crypto_ethereum.balances`** table (or the Google-managed `goog_blockchain_ethereum_mainnet_us`
if the former is stale) — a regularly-refreshed native-ETH balance snapshot, queried
free within BigQuery's 1 TB/month tier and exported to GCS. Fall back to an
**account-only `snap` download** (Nethereum `SnapSyncClient`, verified to exist) if the
dataset is unusable — trustless via Merkle range proofs, ~20–30 GB, storage skipped.
**Rejected:** running a 1.2 TB node (the whole point is to avoid it); Xatu diff-replay
for the snapshot (measured 204–383 GB download, may not reach head — kept only as the
conformance oracle for bounded windows).
**Why:** native balance is a tiny, hash-keyed slice of state; a node drags in storage
and history we never read. BigQuery is the "download the answer" path; snap is the
"trustless self-host" path. Full analysis and the verified probes in
[`docs/data-acquisition.md`](../data-acquisition.md).
**Decision gate before building ingest:** one `bq` query confirms freshness *and*
returns `count(*) WHERE eth_balance > 0`, which fixes the geometry. Needs a GCP free-tier
account; cannot be run from this environment.
**Staying current:** RPC block stream (`prestateTracer ⊕ block.withdrawals[]`), or SQD
state diffs, or BigQuery's own refresh — all trivial at ~300 rows/block.

### ADR-0015 — Store only nonzero balances; `0x0` by absence **[NEW]**

**Chosen:** the database holds only accounts with **nonzero** balance. A lookup that
misses the filter returns `0x0`.
**Rejected:** storing all ~300M ever-seen accounts (most now zero).
**Why:** `eth_getBalance` cannot distinguish a nonexistent account from an existing
zero-balance one — both are `0x0`. So zero-balance accounts carry no information and
need not be stored; their absence *is* the correct answer. This is exact, and it
(a) shrinks the set from ~300M to ~100M, halving the server to ~9 GB, and (b) removes
the bounded-universe honesty gap (ADR-0013), because with a complete nonzero set,
absence unambiguously means "balance is zero".
**Interaction with the false-positive caveat:** a cuckoo false positive can still make
an absent account return garbage instead of `0x0`. ADR-0009's 64-bit-effective
fingerprint drops that to ~2⁻⁶⁰; the conformance run includes nonexistent addresses to
bound it. Nonzero-only storage removes the *systematic* gap; ADR-0009 handles the
*probabilistic* one.

### ADR-0017 — Revisit of ADR-0015 (user request): keep nonzero-only; close the delete hazard with *verified* store ops **[NEW]**

**Context:** the user flagged ADR-0015 ("store only nonzero balances") as possibly
wrong, proposing to store **all** account-balance pairs instead. The unease is
justified — but the diagnosis points elsewhere.
**The real hazard:** `CuckooKVStore::{get, update, delete}` match a slot on the
**32-bit fingerprint alone, first match in probe order wins**. The live feed
routinely emits `(addr, 0)` for accounts *not* in the store (touched by a block
while staying at zero), and delete-on-zero would hand exactly those to `delete` —
which, at ~`arity · bucket_size · 2⁻³²` per absent-key delete, eventually destroys
a fingerprint-colliding **foreign** account's entry (~once a year at mainnet change
rates). That account then silently reads `0x0`: the precise failure the invariant
forbids. Crucially, **store-all does not fix this class** — `update` carries the
identical fp-only first-match hazard, and a zero-balance universe still needs
inserts for never-seen accounts — it only removes deletes while tripling the set
(~300M ever-seen vs ~100M nonzero ⇒ ~36 GB vs ~12 GB server RAM) and buying nothing
semantically (`eth_getBalance` answers `0x0` for absent and zero alike; ADR-0015's
exactness argument is untouched).
**Chosen:** keep nonzero-only, and route **every** key-addressed store op through a
verified candidate-bucket scan (`risepir-server`'s `verified` module) — the
server-side twin of the client's joint fp ∧ `key_tag` mask (ADR-0009):
- key's own entry is the first fp-match (`Own`) → `update`/`delete` provably act on it;
- `Absent` → delete is a **no-op** (even with foreign fp-matches present — the case
  upstream would mis-delete), update falls through to `insert` (writes only empty
  slots; cannot corrupt anyone);
- `Shadowed` (foreign fp-match earlier in probe order) / `DuplicateTag` → loud
  `FingerprintAmbiguity`, block rejected; checksum-corrupt slot → loud
  `CorruptStoredValue`. Erroring is fine; silence is not.
**Rejected:** storing all pairs (above); changing upstream (`delete`/`update` by
fp+tag) — closable in our own layer with public API only, per the ADR-0001 posture.
**Evidence:** deterministic birthday-search tests engineer a real
`(fp, first-bucket)` collision and pin both directions: upstream `get(A)` returns
B's bytes and upstream `delete(A)` destroys B's entry (the hazard is real), while
the verified path leaves B intact, disambiguates both reads, rejects the shadowed
write loudly, and recovers once the shadow clears.
**Cost:** one extra `O(arity · bucket_size)` scan per mutation — noise next to the
per-block hint patch. **Residual:** a `Shadowed`/`DuplicateTag` event rejects its
block and needs a re-bootstrap to clear — years apart in expectation, and loud.

### ADR-0018 — Withdrawal credits are relative amounts, resolved inside `apply_block` **[NEW]**

**Chosen:** `BlockUpdate` gains `credits: Vec<(address_hash, amount_wei)>`
(EIP-4895 beacon withdrawals). `apply_block` applies them **after** all of the
block's absolute `changes`, each as `verified-stored-prior (or 0 if absent) +
amount` through the same verified write path; duplicates accumulate;
`checked_add`/encode overflow rejects the block, never wraps.
**Rejected:** (a) feed-side resolution via archive `eth_getBalance` per recipient —
~16 RPC calls/block on the correctness path for a value the store already holds
authoritatively (ADR-0016); (b) feed-side reads of the server's store — would give
the feed mutable-state access across the one seam (`BlockUpdate`) and race the
applier.
**Why:** withdrawals appear in the block body as *amounts*, not post-balances, so
someone must resolve prior+amount; the server is the only place that is atomic
with the block's own changes (a recipient that is also tx-touched in the same
block must credit on top of the *post-change* value), costs zero RPC, and reads
through the verified scan (ADR-0017), so a colliding foreign entry can neither be
misread as the prior nor overwritten.

### ADR-0016 — Build the KV-SCF / matrix D directly; no intermediate KV map **[NEW — user decision]**

**Chosen:** stream each snapshot pair `(address, balance)` straight into the Segmented
Cuckoo KV store as `fp(address) ‖ value`. That store's flat cell array **is** the
RisePIR database matrix `D`. There is no separate `keccak256(address) → balance` map
and no "build a KV, then convert to `D`" step.
**Rejected:** materialising an intermediate `HashMap` (or on-disk KV) and converting it
to `D`.
**Why:** the two are the same object — `CuckooKVStore` already *is* a keyed store whose
`as_cells()` is `D`, and `RisePirServer` already reads `D` straight from it. An
intermediate map would be a redundant second copy that can drift, and it is exactly the
thing ADR-0011 now drops. Snapshot ingest becomes `for (addr, bal) in snapshot:
store.insert(keccak(addr), encode(addr, bal))`, and a balance change is one
`store.update` / `insert` / `delete` (ADR-0015). The store's key lookup also serves the
feed's own reads (withdrawal prior-value), reliable at the 64-bit-effective fingerprint.
**Consequence:** the store discards addresses after hashing, so recovery at a larger
size comes from the external snapshot (ADR-0011/0014), not from local state. Accepted —
the snapshot is authoritative and re-fetchable, and `TableFull` is years away at ~75%.

### ADR-0019 — The web front end builds its PIR query **in the browser**, in wasm **[NEW — user-scoped]**

**Chosen:** compile the existing `risepir-client` rewind client to
`wasm32-unknown-unknown` as a new workspace member (`crates/risepir-wasm`) and run
query construction, response rewind, and the bucket scan **in the page**. JavaScript
(`web/pir.js`) performs every fetch; the wasm module performs every cryptographic
operation and **never touches the network**. The server (`--web <dir>`) serves the
page from the *same origin* as the PIR transport.

**Rejected — the naive web architecture:** browser POSTs an address to a backend, the
backend runs the PIR query and returns the balance. This is the default shape of every
web app and it destroys the entire point: the backend learns the address. It would be a
fast balance API wearing this project's name. Not built, not offered as a mode, not
reachable by a flag.

**Rejected — browser as a thin UI over a user-run local helper.** `risepir-rpc client
--pir-url …` already puts the rewind client on the user's machine, and a page talking
to `localhost:8545` would inherit that. It remains the *strongest* option and the docs
point at it — but it does not answer the question asked ("serve users on the web"),
requires installing a Rust binary, and does not actually remove the trust discussed
below: the page would still choose what to send to the helper.

**Rejected — `wasm-bindgen`.** The data crossing the boundary is byte buffers and one
`u128`. A plain `cdylib` with C-ABI exports needs no code generator, no version-matched
CLI, and no `npm`: `cargo build --target wasm32-unknown-unknown` *is* the build, so the
artifact anyone audits is the artifact that ships. The whole host ABI is ~20 functions
over two Rust-owned buffers.

**Rejected — a separate repo.** The client must stay pinned to the server's IKPIR rev,
`Geometry`, wire codec, and `ValueCodec` seeds. In-workspace makes that a build error;
cross-repo makes it a *wrong answer*. Sharing `risepir-http::wire` (behind
`--no-default-features`, so no axum/reqwest) rather than hand-writing a JavaScript
decoder is the same argument one level down.

**Why the browser is viable at all — measured, not assumed.** Both risks flagged when
this was only a hypothesis were checked before any of it was built:

| | measured |
|---|---|
| Does the stack even compile to wasm? | Yes, with `parallel` (rayon) off — it has no threads to fan out to and does not degrade gracefully. 157 KB `cdylib`. |
| Client compute, per lookup (3 segments), single-threaded, no SIMD, no `target-cpu=native` | **4 ms** @100 k · **10 ms** @1 M · **27 ms** @9.4 M accounts |
| `client_setup` (expanding `A` from its seed) | 0.2–0.5 s |
| First-load hint download (`Geometry`, computed) | 16.5 MB @100 k · 49 MB @1 M · 99 MB @4 M · 140 MB @9.4 M |

So latency — the risk that would have killed this — is a non-issue; SimplePIR client
work is a rounding error next to the network. The binding constraint is the one-time
hint download, and it is a **product** limit, not a cryptographic one.

**Consequence — the scale ceiling, stated plainly.** At the complete mainnet nonzero
set (~100 M accounts) the hint is **588 MB** and the client needs ~1.2 GB of RAM. That
is not a web page. The browser client is honest at demo/partial scale (the deployment
this ships with runs `--partial-capacity 1000000` ⇒ 49 MB); at full mainnet scale the
answer is `risepir-rpc client` on a machine that can hold it, paying the download once
and keeping it. The web front end does not pretend otherwise.

**The residual trust, which is real and is stated on the page itself.** PIR guarantees
the server learns nothing about *which* account was queried — but the code that builds
the query is delivered by that same server. Whoever serves the page chooses what the
client does; over plain HTTP, so does anyone on the network path. This is the classic
browser-delivered-crypto problem and it is not solved here, it is *disclosed*: the page
says so, in the "What this does not protect" section, above the fold of the limits, and
points at the local client for anyone who needs more. Three things narrow it:

- **Same-origin + `connect-src 'self'` CSP.** The page is structurally unable to POST
  anywhere but the server it already talks to. That constrains a *tampered page*; it
  cannot constrain a *dishonest origin*, and the ADR does not claim it does.
  (`'wasm-unsafe-eval'` is also required — Chromium classes wasm compilation as script
  evaluation — which permits wasm only, leaving `eval`/`new Function` blocked.)
- **The wasm module's only import is `env.risepir_fill_random`.** No fetch, no clock,
  no storage. It is structurally incapable of exfiltrating anything itself, and
  `web/test/e2e.mjs` asserts that import list so it stays that way.
- **The same Rust, both places.** The wasm client is `risepir-client`, the crate the
  CLI client uses, so "audit the client" is one audit.

**Not covered, also stated on the page:** network metadata. PIR hides *which account*,
not *who is asking, when, how often, from where*. The client works fine over Tor and
the page says so.

**Entropy is a build error, not a runtime hope.** The LWE secret comes from
`rand::rng()` → `getrandom`, which has no ambient source on `wasm32-unknown-unknown`
and **refuses to compile** unless a backend is named. That refusal is load-bearing: a
client that silently fell back to a fixed seed would return perfectly correct balances
while letting the server subtract `A·s` and read the queried bucket straight out of the
query — correct-looking, silent, total failure. So `.cargo/config.toml` pins
`getrandom_backend="custom"` for that target and `crates/risepir-wasm/src/entropy.rs`
routes it to the host's `crypto.getRandomValues`; the import is mandatory, so a host
that forgot to wire it gets an instantiation crash, not a downgrade. Tests assert
repeated queries for one address ship *different* ciphertext of *constant* length.

**Consequence — `parallel` became a feature everywhere.** `ikpir-common` is now
inherited with `default-features = false` at the workspace root (a member cannot
subtract a default the workspace switched on), and every crate re-enables it through
its own **default-on** `parallel` feature. Every existing build is unchanged; only the
wasm client opts out. A new crate that depends on `ikpir-common` must add the same
forwarding feature or it silently builds the scalar kernels the headline numbers were
not measured against — noted in the manifest.

**Deferred deliberately — caching the hint across visits.** A cached hint stays valid
only while the server retains the deltas from its pinned block (`DeltaRing`, and
`range` is strict: any missing block ⇒ `409` ⇒ the client must re-download rather than
guess). That mechanism *is* sound enough to cache against — a re-bootstrapped server
starts an empty ring, so a stale-lineage hint gets a `409` rather than a wrong answer —
but it is one more correctness argument standing between a user and a balance, for a
UX win that a local deployment does not need. Not built; the requirement for building
it safely is recorded here.

**`GET /recent` (new endpoint).** A partial deployment only knows accounts touched
since bootstrap, so a visitor typing an arbitrary address gets an honest error and
learns nothing about the system. `/recent` serves up to 128 recently-touched addresses
— public chain data, identical for every caller, and address-free as a request. It does
not weaken PIR: the server still cannot tell which entry, if any, was then queried, and
the page says exactly that rather than leaving it to be inferred.

### ADR-0020 — Threat model: documented trust assumption + operator-side detection, not verifiable PIR **[NEW]**

**Chosen:** write the adversary definitions down ([`docs/threat-model.md`](../threat-model.md))
and adopt, explicitly, the *documented honest-but-curious operator* posture: the
system's integrity mechanisms (ADR-0005/0009/0017) defend against **accident**;
against a **malicious operator** the defense is disclosure plus operator-side
detection (the sampled reconcile), with signed store digests → public anchoring →
full verifiable PIR (VeriSimplePIR-style) recorded as future rungs, none of which
stop *targeted* lying to one user.
**Rejected:** (a) implementing verifiability now — for SimplePIR-based schemes this
is a research effort (plausibly a second paper), and a Merkle-proof-per-answer
"fix" defeats the system outright because the proof path names the account;
(b) leaving the assumption implicit, which is how PoCs quietly overclaim.
**Why:** the roadmap's A1 open question — "full verifiability, or a documented,
honestly-stated trust assumption plus detection?" — needed an explicit answer
before any other hardening is prioritizable; both are defensible for a PoC and
the second is the one that keeps the artifact honest *today*. The threat model
also pins two operational rules that were previously folklore: the reconcile
check is operator-side only (client-side per-query use reveals the address), and
plaintext HTTP widens "the operator" to "anyone on-path", making TLS a
prerequisite for naming a trusted party at all.

### ADR-0021 — CI: GitHub Actions with a read-only deploy key for the private primitive **[NEW]**

**Chosen:** GitHub Actions (`.github/workflows/`): clippy `-D warnings` + full
workspace tests on every push/PR; the network-free `xtask conformance` gate on
PRs; the live feed gate + short coverage-guided fuzz runs nightly (network-
dependent and flaky-by-nature jobs never gate merges); `cargo-deny` for
advisories/licenses/sources. The private `bao-ninh-orochi/IKPIR` dependency is
fetched with a **fine-grained PAT** scoped to that one repo, Contents
read-only, stored as an Actions secret (`IKPIR_TOKEN`) here and wired through
an `insteadOf` rewrite + `git-fetch-with-cli`. A missing secret fails with an
explicit `::error` naming this ADR, not a cryptic fetch error.
**Rejected:** a read-only deploy key — the *preferred* least-privilege shape,
but **deploy keys are disabled on the IKPIR repo** (verified: `gh repo
deploy-key add` → HTTP 422 "Deploy keys are disabled for this repository",
2026-07-21 — the same constraint deploy.md §3.5 hit for the VM build);
vendoring IKPIR (drifts from the pinned rev and bloats the repo);
`cargo fmt --check` in CI *now* — the tree predates
`rustfmt.toml` (296 files of drift) and a mechanical reformat while the web
branch is in flight would manufacture conflicts, so the gate turns on in a
formatting-only commit after in-flight branches land.
**Why:** 183 tests, a conformance harness, and a live gate existed and ran only
when a human remembered — the highest impact-per-hour item in the roadmap. The
deploy-key choice is the standard least-privilege answer to "CI must read a
private sibling repo".

### ADR-0022 — cargo-deny runs on the host, not in the Docker action, so it sees the IKPIR credential **[NEW]**

**Chosen:** run `cargo deny check` directly on the runner, with cargo-deny
installed via `taiki-e/install-action` (SHA-pinned). It then inherits the exact
host git config the clippy/test job uses — the `insteadOf` rewrite written by
the "Authenticate the private IKPIR dependency" step plus
`.cargo/config.toml`'s `git-fetch-with-cli` — so its `cargo metadata` fetch of
the private IKPIR dep authenticates like every other job.
**Rejected:** `EmbarkStudios/cargo-deny-action` — ADR-0021's original choice. It
runs cargo-deny **inside a Docker container** with its own `$HOME` (`/github/home`),
so the credential the auth step wrote to the *host* git config is invisible; the
container's git-fetch-with-cli then fails with `fatal: could not read Username
for 'https://github.com'` and cargo-deny reports `failed to fetch crates`.
Also rejected: injecting credentials into the container via the action's inputs
— a second auth path to keep in sync with the two host jobs, for no benefit.
**Why:** the first CI run after `IKPIR_TOKEN` was finally provisioned (2026-07-22)
exposed it — clippy+tests went green and conformance skipped correctly, and the
*only* red job was the Dockerized deny, failing on git auth rather than on any
advisory/license/source finding. Moving it host-side puts every cargo-touching
job on one identical auth mechanism, which is the invariant ADR-0021's wiring
assumed but the container silently broke. [DEVIATES from ADR-0021]
