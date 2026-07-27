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
it safely is recorded here. (ADR-0028 later built the *server*-side half of this idea —
one shared, cached `/setup` encode reused across requests, on this same
DeltaRing-bridgeability argument; a client-side persistent cache across browser visits
remains deferred.)

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

### ADR-0023 — The complete set needs a 64 GB box: measure the account count before sizing anything **[DEVIATES from deploy.md §2.3]**

**Chosen:** run the complete mainnet set on a **64 GB** machine (GCP
`e2-highmem-8`, 8 vCPU, 250 GB disk), and treat the §2.1 gate query's
`nonzero_accounts` as the *only* input to sizing.
**Rejected:** the "16 GB floor, 24 GB comfortable" the runbook had carried since
Stage 1 planning, and with it the Oracle Cloud Always Free 24 GB box that §2.3
named as the $0 way to run this.

**Why:** the gate query had never actually been run — §2.3's numbers came from an
estimated "~100–130 M nonzero accounts". Run for real on 2026-07-26 it returned
**200,503,969**. That is not a rounding difference: `Geometry::for_accounts`
rounds the segment count up to a power of two, so 200.5 M needs 3 × 2^25 =
100,663,296 buckets, 22 cells per slot at `plaintext_bits=8`, and a server DB of
**35.43 GB** — about 3× the estimate, and above every free tier that existed.
Provisioning from the old table would have meant paying for a 5.6 GB export and
a 12-minute ingest before dying at allocation.

Two consequences worth stating separately, because each is a trap on its own:

- **The state save had to be fixed first.** `state::save` obtained the cells via
  `snapshot_cells()`, which *copies* the array. At 35.43 GB that takes peak RSS
  from ~38 GB to ~73 GB — so the save, not the serving, would have set the
  machine size, and on a 64 GB box the process would have OOMed at the moment it
  tried to persist a bootstrap that had already cost 25 minutes. Fixed in PR #6
  by adding a borrowing `RisePirServer::cells()` and streaming from it; the
  file format is byte-identical.
- **Headroom is a step function, so budget for the step.** Load is only 0.498
  because of the power-of-two rounding, which means account growth to
  **301,989,888** is free — same geometry, same 35.43 GB, load arriving at
  exactly the 0.75 target — and the very next account doubles the DB to 70.9 GB.
  A 48 GB box would serve today's mainnet and fall over at that boundary with no
  warning; 64 GB buys the years in between, not a safety margin.

The general rule this encodes: **derive geometry from a measured count, never a
remembered one.** The server prints its geometry line before it allocates, and
that line — not any table in this repo — is the commitment.

### ADR-0024 — The feed is an ordered endpoint list, not one URL; and the client sends a User-Agent **[NEW]**

**Chosen:** `RpcFeed` takes an **ordered list** of JSON-RPC endpoints and walks
it per call, with `https://eth.drpc.org` primary and `https://eth.merkle.io`
behind it; `--feed-url` is repeatable. `RpcClient` identifies itself as
`risepir-rpc/<version>`.
**Rejected:** a single feed URL (what shipped through 2026-07-25); and making
every endpoint's reachability fatal at startup.

**Why:** a single endpoint is a single point of *permanent* failure, not a slow
one. Keyless providers refuse **individual** heavy blocks on plan limits,
deterministically — dRPC serves `debug_traceBlockByNumber` in ~1 s for most
blocks and answers `HTTP 408 "Request timeout on the free plan"` for some, every
time it is asked. The follow loop may never skip a block (a skipped block is a
wrong balance), so it retries forever, and the deployment stops advancing for
good. Observed 2026-07-26: the complete-set deployment wedged at block
**25,613,828** through 55 identical retries, having served the previous 594
blocks fine — about one refusal per 600 blocks, i.e. a dozen a day at mainnet's
rate.

Of 17 keyless endpoints surveyed on the exact block dRPC refuses, **one** served
it: `eth.merkle.io` (200, 7.2 MB, 1.04 s). It rate-limits under sustained load,
which is precisely why it belongs *behind* dRPC — at one block in 600 it is
never under load. The others were rate-limited, unauthorized, or did not
whitelist the method.

Two failure modes this turned up, both worth keeping in mind beyond this repo:

- **`reqwest` sends no `User-Agent`, and Cloudflare-fronted endpoints 403 that.**
  Every check that qualified merkle.io was run with `curl`, which always sends
  one — so the endpoint benchmarked perfectly by hand and was unusable from the
  binary, which is the worst possible combination for diagnosis. **Validate an
  endpoint with the client that will actually call it.**
- **Strict startup verification made the fallback worse than none.** The first
  cut verified `eth_chainId` on every endpoint and treated any failure as fatal,
  so a transient 403 from the *backup* aborted startup on a server holding a
  36 GB state file that had cost 33 minutes to build. Strictness is now by
  position: a chain-id **mismatch** is fatal anywhere (another chain's blocks
  would corrupt the database), an unreachable **primary** is fatal, and an
  unreachable **fallback** is a warning — it stays in the chain marked
  unverified and is chain-checked before any of its data is believed, so
  "temporarily down" never becomes "silently trusted".

**Trust note:** the default feed set is now two operators rather than one. Both
see only *which blocks* this server fetches — never which account a user
queried, which is the PIR property and is unaffected. Reconciliation still runs
against publicnode, a third independent operator, so the never-wrong-answer
backstop does not share an operator with either feed.

### ADR-0025 — Periodic state autosave, executed by the follow loop itself **[NEW]**

**Chosen:** rewrite the `--state` file periodically **from the follow-loop task,
between block applications**, under the existing read lock: new `--save-interval
<secs>` (default **1800**, `0` disables), interval measured from the previous
save's *completion*, skipped when no block was applied since the last save
(`StateSaver`, `crates/risepir-rpc/src/autosave.rs`). The shutdown save now goes
through the same `StateSaver`, whose mutex serializes it against an in-flight
autosave; `state::save` additionally fsyncs before the rename and reports
size/duration (the bootstrap "saving state …" line finally has a completion
line). File format unchanged — still `RPST2`, existing files load as before.
**Rejected:** a periodic save on its own timer task; snapshotting the cells for
a consistent copy; an incremental delta-journal sidecar; `fork()`-based
copy-on-write snapshots; documenting an external periodic restart instead.

**The problem, measured** (live deployment, 2026-07-26): the state file was
written exactly twice — bootstrap and Ctrl-C — so it drifted from the running
server without bound: file at block 25,613,849 (12:37 UTC) vs 25,617,496 in
memory (14:28 UTC) = **3,647 blocks stale in under two hours**, growing ~300
blocks/hour. At the replay rate of ~50 blocks/min, a kill -9 at that moment
cost ~73 minutes of catch-up, +~6 min per further hour of uptime. Nothing
wrong ever gets served — the gap is purely recovery time — but it is what
stood between the deployment and being safely unattended.

**Why the save must run in the applier's own task** — the fact that shaped
everything: `tokio::sync::RwLock` is **fair (write-preferring)**, so the moment
`apply_block` queues its write behind a long-held read guard, every *later*
`/answer` reader parks behind the writer too. A full save streams 36.26 GB
under the read lock — minutes at the complete set — so a save from a separate
task turns "the follow loop waits" into "**serving stalls for the whole
save**", every interval. Pinned executable
(`tests/autosave.rs::queued_writer_parks_new_readers`): with one reader held
and one writer queued, `try_read` fails. The dual fact: `NodeState`'s write
lock is taken in exactly one place (`node.rs` `apply_block`), and its only
caller is the follow loop — so a save executed *by that task, between blocks*
can never have a writer queued behind it, and readers flow for the entire
save. The trigger sits at the top of both loop bodies, so it fires while
caught-up, mid-catch-up, and during a refused-block retry (ADR-0024), and
always after the previous iteration's reconcile — a state that just failed
reconciliation is never persisted (the loop exits first).

**Consistency is carried by the read guard, not by the placement:** writers
are excluded for the whole streamed save, so `D` and `H` land at one height no
matter which task saves. Placement is purely a liveness choice. Test
(`concurrent_saves_reload_consistently`): an applier task and a saver task
run concurrently — the *adversarial* arrangement production avoids — and all
40 interleaved saves reloaded byte-exact: verified reads equal to an
independent plain-arithmetic simulation at each file's own height, and replay
to the final height byte-identical (cells *and* encoded hints) to the live
server. The per-block withdrawal credit makes partial-block captures
non-idempotent, so a mid-block tear cannot cancel out.

**Cost accepted:** the follow loop pauses for the save's duration (it then
catches up — the same self-healing lag as after any restart; measuring the
interval from save *completion* guarantees forward progress even if an
operator sets the interval below the save duration), and the disk absorbs a
full file rewrite per interval (~1.7 TB/day at the default on the complete
set — persistent disks bill capacity, not writes). Worst-case on-disk
staleness ≈ interval + one save duration; at the default that is minutes of
replay after a kill -9, versus unbounded before. Peak RSS is untouched: the
autosave calls the same streaming `state::save` (borrowed cells — the PR #6
constraint) as the Ctrl-C path, so an autosave's peak equals a shutdown
save's peak.

**Why the rejections:**
- *Separate timer task:* the fairness stall above — a minutes-long serving
  outage per save at complete-set scale.
- *Cell snapshot for consistency:* re-creates the +35.43 GB copy PR #6
  removed; OOMs the 64 GB box (ADR-0023).
- *Delta-journal sidecar (incremental checkpoints):* tiny writes and ~zero
  staleness, but it adds a second persistence format and a second
  reconstruction path, both of which can silently produce exactly the
  inconsistent-server outcome this repo treats as total failure (a journal
  entry applied to the wrong base, a torn tail mis-handled, hint-vs-cell
  drift in replay). The full-file save reuses one battle-tested
  format+loader whose checksum already rejects everything else. Bounding
  recovery to minutes is enough; buying ~zero at that risk is a bad trade
  here. Revisit only if the save duration itself becomes the problem.
- *`fork()` copy-on-write (the Redis BGSAVE trick):* the textbook zero-copy
  consistent snapshot, but forking a multithreaded tokio process leaves the
  child restricted to async-signal-safe operations — a Rust `File`/allocator
  call in the child after fork is UB-adjacent if another thread held a lock —
  and it is unavailable on non-unix. Too sharp a tool for a PoC.
- *External periodic clean restart:* bounds staleness only to the restart
  cadence, costs 135.8 s of downtime per restart plus operator machinery,
  and the in-process save turned out to cost nothing it was competing with.

**Residuals, stated:** (1) reconcile samples every `--reconcile-every` blocks,
so a feed drift can be persisted up to that many blocks before detection — the
same window in which it is *served*; the posture (CRITICAL + re-bootstrap) is
unchanged, and the autosave stopping with the follow loop means the file
freezes at the last pre-condemnation save. (2) A shutdown save after a
CRITICAL apply-failure can persist a state whose hints lag a partially-applied
block's cells — **pre-existing** Ctrl-C behavior, now recorded: the load-time
checksum cannot see it (the file faithfully records the inconsistent memory);
the operator instruction after any CRITICAL remains "re-bootstrap, do not
trust the state file". (3) `ops/systemd/risepir.service` `TimeoutStopSec` rose
300→900: a SIGINT can now land mid-autosave and must wait for it before the
final save; two complete-set saves must fit the stop window.

### ADR-0026 — Sidecar delta journal beside the state file, restore opt-in behind a soak period **[NEW — revisits ADR-0025's rejection]**

**Chosen:** an append-only `<state>.journal` (format `RPJL1`) recording one
`BlockDelta` + `num_items_after` per applied block, header-bound to a specific
base file by its `RPST2` trailing xxh3 digest (`state::SaveReport::digest`).
Writing is **always on** once a first full save exists (`StateSaver` owns the
current `JournalWriter`, rotating it — a fresh `create` bound to the new
digest/height — strictly after each successful save's rename). Restoring from
it is **opt-in**, `--journal-restore` (default off): off, the journal is only
*scanned* read-only at startup and reported (`journal intact: N records to
block X (--journal-restore to use)`) — the soak signal an operator watches
before trusting it; on, every valid record is replayed onto the raw
cells/hints *before* the store is constructed, so the server resumes at the
journal's height instead of the base file's.

**Rejected (this repo, again):** everything ADR-0025 rejected remains
rejected for the same reasons (a separate timer task's fairness stall, a
cell-snapshot copy, `fork()` COW, external periodic restart) — see that ADR.
Also rejected here specifically: **content-defined-chunking dedup of the full
image** (would still cost a diff pass over tens of GB per interval to find
what changed — the semantic delta is already known for free, from the same
fold step that patches the hint); **a mutable in-place state image**
(overwriting slots as blocks apply removes the atomic-rename safety net
entirely — a crash mid-write corrupts the *only* copy — so it would need a
write-ahead log of its own to be crash-safe, which is this journal by another
name, minus the existing battle-tested full-file format as a recovery
floor); **filesystem/persistent-disk snapshots** (a snapshot is a point the
*infrastructure* chooses, not one `apply_block` chose — it can land mid-write
to the state file's `.tmp`, and even a clean snapshot only ever recovers to
some past full-save-equivalent, buying nothing over the full save alone);
**fork/COW** (unchanged from ADR-0025 — still unavailable on non-unix, still
UB-adjacent for a multithreaded tokio process).

**Why the previous rejection is now reversed:** ADR-0025 rejected exactly this
shape — *"it adds a second persistence format and a second reconstruction
path, both of which can silently produce exactly the inconsistent-server
outcome this repo treats as total failure (a journal entry applied to the
wrong base, a torn tail mis-handled, hint-vs-cell drift in replay)"* — and
that risk was real, not hypothetical caution. This design does not dissolve
it; it **contains** it, with the containment itself being the ADR:

1. **A journal entry can never apply to the wrong base.** The header commits
   to the exact base digest at creation time; a loader that does not see a
   matching digest never touches a byte of the payload (`base_mismatch_*`
   tests, `journal.rs` / `tests/journal.rs`) — "wrong base" degrades to
   "journal ignored", never to "journal misapplied".
2. **A torn tail cannot mis-handle itself into corruption.** The reader is a
   streaming validator with two failure classes, never three: *pre-apply*
   (bad checksum, decode error, height gap, truncated/oversized record) always
   **stops at the last good record** and reports where — the scan never
   errors the whole read, and it never skips a bad record and continues past
   it (`ScanStop::Invalid`, pinned by five dedicated tests: torn tail,
   mid-file bit flip, height gap, oversized length, a length exceeding the
   remaining file size). *Apply-time* (the `0 <= cell + Δ < p` bound —
   ADR-0005's own integrity check, now checked during replay instead of only
   during live application) is the one failure this repo treats as
   unrecoverable **from inside the replay**, and it is handled by refusing to
   serve at all (`RestoreError::ApplyFailure` → `die()`) rather than guessing
   which prefix is still trustworthy — happening before serving starts, per
   this repo's standing rule, dying is the honest move.
3. **Hint-vs-cell drift cannot happen** because both are patched from the
   *same* decoded record in the same loop iteration, via the same
   `server_patch_hint` call the live `apply_block` path uses — replay is not
   a second implementation of "how a block changes the database", it is the
   same fold-and-patch step already measured against a real `IkpirServer`
   oracle (ADR-0004's `batched_equals_per_mutation`), now driven by a decoded
   `BlockDelta` instead of a fresh mutation-log drain.
4. **A restore is never trusted silently.** `--journal-restore` is off by
   default; off, the journal only ever gets *scanned*, and the loud report
   line is the mechanism for building confidence in a specific deployment's
   journal before flipping the flag — the soak period ADR-0025's authors did
   not have when they wrote "revisit only if the save duration itself becomes
   the problem." (It has: at the complete mainnet set a full save is minutes
   long and costs ~1.7 TB/day of writes at the default interval — the
   motivating problem this ADR exists to relieve, once the operator trusts
   the soak evidence enough to raise `--save-interval` to hours.)

**The `H`-churn argument (why a journal buys anything at all):** `server_patch_hint`'s
documented contract makes `H` a *deterministic function of the delta stream* —
replaying the same `BlockDelta`s reproduces the identical hint bytes a live
server would have, bit for bit (this is exactly what
`journal_replay_matches_live_apply` pins). So nothing is lost by persisting
the ~15–30 KB/block *semantic* delta instead of the ~25–40 MB/block of literal
`H` + cell-array byte churn a periodic full save re-writes wholesale — the
journal is not a compression trick over the full save, it is the observation
that most of what a full save re-writes did not need writing again at all.

**`num_items_after` rides on the wire because replay cannot derive it.** A
delete and an insert both look like "some cells changed" to the fold step —
nothing in a `BlockDelta` distinguishes "this row went from occupied to
empty" from "this row's fingerprint changed" without re-deriving the whole
cuckoo placement logic during replay. Carrying the true post-block count
avoids that duplication entirely and sidesteps whatever validation
`Segmented3aryCuckooKVStore::from_cells` might someday grow: **verified in
the local RisePIR checkout that today it performs none** — `num_items` is
"not validated against `cells`; trust-on-restore" (`store.rs`'s own doc
comment) — so the true count is not required for `from_cells` to succeed, but
it is required for `num_items()` to *report the truth* afterward, which this
repo's balance-correctness posture demands regardless of what any one
version of the store happens to check.

**Rotation ordering is load-bearing, and is a known, accepted residual, not a
hidden gap:** the base save's rename is the commit point; journal rotation
(`JournalWriter::create` against the *new* digest) runs strictly after. A
crash in the narrow window between them leaves `(new base, old journal)` —
digests that do not match — which a restart's loader detects
(`header.base_digest != loaded.digest`) and ignores loudly, falling back to
the base alone. The window is real but its failure mode is exactly
"journaling degrades to the previous save's cadence for one cycle," never a
wrong answer.

**Adoption rules (when an existing on-disk journal is trusted to keep
appending, versus left untouched):** an appender is only ever adopted in two
shapes — restart-time (either restore mode) once a journal's header is
confirmed to match the loaded base — and freshly `create`d right after a
snapshot bootstrap's own first save. With `--journal-restore` off and the
journal *ahead* of the loaded base (the base file records block B, the
journal's last valid record is beyond B — the ordinary state after any
kill -9 between autosaves), the journal is deliberately left **untouched**:
adopting it would set the writer's continuity state past B, and the very
next live append (B+1) would then look like a gap to a writer that thinks it
is already past B+1 — so this run journals nothing until the next save's
rotation starts a fresh file at whatever height that save lands on. The file
itself is never deleted or overwritten in this state — it is someone's
recovery data until `--journal-restore` says otherwise.

**Evidence.** Unit-level (`crates/risepir-rpc/src/journal.rs`, 14 tests):
header corruption/checksum mismatch, base mismatch, torn tail, mid-file bit
flip, height gap, oversized length (both the absolute `MAX_RECORD_BYTES` cap
and the independent remaining-file-size bound), an empty (header-only)
journal, `append`'s own gap refusal, `adopt`'s tail-truncation-and-resume.
Integration-level (`crates/risepir-rpc/tests/journal.rs`, 7 tests):
`journal_replay_matches_live_apply` — the load-bearing one — replays a real
journal (written by a real `JournalWriter` fed real `NodeState::apply_block`
deltas, including a genuine delete and a withdrawal credit) through the real
`state::load_with_journal_restore` path and asserts byte-exact cells, encoded
setup (hints + params + block), `num_items()`, and individual balances
(the deleted account reading back `None`, the credited one matching); the
other six pin torn-tail recovery via `adopt`, corruption stopping replay
exactly there, base mismatch falling back to a plain load, a hand-enforced
gap, tail-deltas seeding `NodeState::seed_history` in the right order and
count, and `u32::MAX`-length hostility never allocating. Fuzz:
`fuzz/fuzz_targets/journal_scan.rs` mirrors `state_load.rs` against arbitrary
bytes. Smoke, on this Mac against real mainnet (`--partial`,
2026-07-27): one autosave + several more blocks applied and journaled, then
`kill -9`; a restart with `--journal-restore` logged
`journal replayed: 11 block(s) in 0.3s — resuming at block 25620745 (base was
25620734)` — 11 blocks recovered that a plain restart would have replayed
from the network instead; a second restart without the flag logged
`journal intact: 13 records to block 25620747 (--journal-restore to use)`
against the same (now further-advanced) journal, correctly identified it as
ahead of the reloaded base, and left it untouched; a final `Ctrl-C` produced
the ordinary graceful shutdown save and rotated the journal to the new
height, ending at a clean, empty (header-only) file — exactly the documented
behavior in every branch, not just the happy path.

**Staged posture, restated plainly:** the payoff configuration is a long
`--save-interval` (hours) plus `--journal-restore` — ~150 MB/day of journal
writes recovering to within seconds of the last applied block, instead of
~1.7 TB/day buying minutes. Nothing here forces that combination; the
default (`--journal-restore` off, `--save-interval` unchanged) is
byte-for-byte today's behavior plus a sidecar file and a report line, which
is the point — the feature must be free to ignore before it is trusted to
lean on.
### ADR-0027 — Make the reconcile check's own health observable; escalate on prolonged darkness, never halt on it **[NEW]**

**Chosen:** track the cross-provider reconciliation check's own health —
`ReconcileHealth` on `risepir-http`'s `NodeState`, guarded by its own
`std::sync::Mutex` on the `recent` field's pattern (never inside the PIR
server's lock). `reconcile` classifies every checkpoint before logging or
recording it: `Empty` (no candidate accounts — the block touched nothing
worth sampling, not a failure of anything), `Success{checked}`, or
`Dark{attempted}` (≥1 comparison attempted, all failed) — and logs all three,
where a dark checkpoint always warns with the attempt/fail count and time
since the last success. `consecutive_dark` increments only on `Dark`, resets
only on `Success`, and is left **unchanged** by `Empty` — an empty block is
not evidence of anything failing and must not be conflated with "checked and
every attempt failed", which is the distinction this whole ADR exists to
make. After `DARK_ESCALATION_THRESHOLD` (20) consecutive dark checkpoints —
~2 h at the default cadence — the crate's `critical(...)` line fires, then
re-fires every further 20 rather than once. `GET /healthz` grows `key=value`
lines for all of the above, with the **first line kept byte-identical to
today's `ok <head>`** so an existing prefix/line probe never breaks, and an
unconfigured deployment (mock/demo, or `--reconcile-every 0`) reports
`reconcile_configured=0` explicitly rather than omitting the fields. A value
mismatch still halts following exactly as before — nothing here touches that
path.

**Rejected:** (a) halting the follow loop after prolonged darkness — the
naive reading of "detection is now blind, so stop." Reconciliation checks an
*independent third party*; halting because that party is unreachable
converts publicnode's outage into this deployment's outage while preventing
exactly zero wrong answers (the feed, and therefore every served balance, is
untouched by whether the reconcile provider happens to answer). (b) a
separate `GET /reconcile` endpoint — a second thing to discover, poll, and
keep in sync with `/healthz`'s own liveness semantics, for data that costs
nothing to fold into the one probe that already exists. (c) JSON on
`/healthz` — every other endpoint in this crate is a deliberately binary
wire format (`docs/plan.md` ADR-0006); `/healthz` is already the one
plain-text exception, and a handful of counters does not justify a second
text format on top of it.

**Why:** the 2026-07-26 complete-set catch-up ran this exact check dark for
~2 hours straight — publicnode's keyless tier refuses archive-depth
`eth_getBalance`, so all 685 checkpoints during the snapshot→head replay
logged a per-sample fetch failure and nothing else (`docs/deploy.md` §5.3,
"silently unavailable"). The old code's summary line was gated on
`if checked > 0`, so a checkpoint with zero completed comparisons — whether
because the block touched nothing (routine) or because every fetch failed
(the actual incident) — produced no log line, and `reconcile` returned `true`
either way. Nothing downstream (`/healthz`, `/mode`, the startup banner, the
page) could tell "checked and exact" apart from "not checked at all". Closing
that blind spot is the entire point; making the follow loop *halt* on it was
never on the table, because that would hand a stranger's uptime the power to
stop this deployment.

**Cost — say the sampling rate honestly:** at the defaults (`reconcile_every
30`, `reconcile_samples 8`) reconciliation runs ~1,920 account comparisons a
day against a complete set of 200.5 M accounts. That is not coverage, and
this ADR does not claim it is — it is a well-targeted smoke test, sampling
exactly the accounts the block just changed, which is where a feed error
would actually show up. `docs/threat-model.md` §6 now states this rate
plainly instead of leaving "sampled" to imply more than it does.

**Follow-up, deliberately deferred:** surfacing any of this on the browser
front end (`web/`) is left to the front-end redesign already in flight on
another branch — out of scope here.
### ADR-0028 — Cache one shared `/setup` encode; the per-route concurrency cap never earned its keep **[NEW]**

**Chosen:** `NodeState` caches one already-encoded `GET /setup` response
(`bytes::Bytes`, refcounted) behind its own `tokio::sync::Mutex`, regenerated
only once the server's head has advanced more than **half** the `DeltaRing`'s
capacity past the block the cache is pinned at. Every `/setup` caller now gets
a clone of that one shared buffer — a refcount bump, not a copy — so
live-buffer memory for this route is **O(1)** in the number of concurrent
downloads instead of **O(N)**. The route's `ConcurrencyLimitLayer` (2 permits)
is removed outright and `SETUP_MAX_CONCURRENT` deleted with it, not renamed or
retuned. `GET /setup` also now sets `ETag`/`Cache-Control` and answers a
matching `If-None-Match` with `304`, and `wire::encode_setup` pre-sizes its
output buffer from the bundle's own geometry instead of growing it.

**Rejected:** holding the permit across the body transfer instead of just the
handler (reintroduces the exact throughput ceiling this change removes — see
the second measurement below); streaming `/setup` from a borrowed hint instead
of caching an encoded copy (does not remove per-connection response
buffering at the layer below this crate, and complicates the wire format for
a saving the cache already gets for free); a long `Cache-Control: max-age`
instead of `no-cache` (a browser could then keep reusing a bundle whose block
has since aged out of the ring and loop on a `409` from `/sync` forever — the
same "silently stranded" failure mode the half-window rule below exists to
rule out).

**Why:** measured against the real binary — **partial mode, a 16 GB laptop,
never at the complete mainnet set's 200 M-account scale**, flagged explicitly
so these numbers are never mistaken for a production measurement:

- **The concurrency cap never bounded what its own doc comment claimed.**
  `tower`'s `ConcurrencyLimit` holds its permit in the *response future*
  (`tower-0.5.3/src/limit/concurrency/future.rs`), and this handler always
  returned a fully-materialized body, so the permit was released the instant
  the handler returned — before a single byte reached a slow client, not
  after the last one did. With 2 curl readers throttled to 20 kB/s
  mid-transfer of a 1.77 MB mock `/setup`, a 3rd and 4th request each
  completed in ~1.5 ms carrying the *full* body; repeated in partial mode
  (a 48,960,201-byte `/setup`) with 20 throttled readers already in flight, a
  21st request still returned the complete body, with a 22 ms
  time-to-first-byte. The task brief that prompted this work claimed a slow
  reader "can occupy a slot indefinitely" and that two slots cap the
  deployment at "~15 new clients/hour" — **both claims are false**, which is
  worth saying plainly rather than silently fixing alongside everything else.
- **The real hazard was the mirror image, and was bounded by nothing.**
  Because the permit released before transfer while each request's own
  encoded body stayed alive for that response's whole lifetime, the number of
  live ~831 MB buffers had no bound at all. RSS climbed monotonically with
  concurrent throttled readers in partial mode (baseline ~100–110 MB),
  reaching **631 MB with 40 readers in flight**. Allocator retention makes
  any single per-reader figure noisy, so only the observed totals are
  recorded here, never a derived per-reader constant.
- **Half the ring, not all of it.** A client that bootstraps from a bundle
  pinned at block `B` must reach `GET /sync?from=B&to=head` before `B` ages
  out of the `DeltaRing`'s retention window, or it is stranded on a `409`
  with no recourse but a full `/setup` re-download (`DeltaRing::range` is
  strict by design — ADR-0006/0015's "never silently omit part of the
  requested range" applied to the retention window). Reusing the cache for
  the ring's *entire* window would let it serve a bundle that is already too
  stale to finish syncing the moment the download completes: an 831 MB
  transfer took ~8 minutes measured end to end, and the ring's capacity is
  sized in blocks, not in wall-clock slack for a slow client. Capping reuse
  at half the window — at the deployed 600-block ring, ~1 hour of chain time
  — reserves the rest for exactly that download-plus-catch-up, while still
  amortizing the encode over roughly half the ring instead of every block.
  [`DeltaRing`] gained a `pub const fn capacity(&self)` accessor so this
  arithmetic never has to duplicate the constructor's argument.
- **`ETag` / `304`, honestly caveated.** `GET /setup` sets
  `ETag: "setup-<block>"` and `Cache-Control: no-cache` (always revalidate,
  never "don't store"); a matching `If-None-Match` gets `304 Not Modified`
  with no body. Checked against the same freshness decision that already
  gates the `200` path rather than a second, separately-maintained check, a
  `304` is therefore only ever returned for a bundle the ring can still
  bridge forward — correct by construction, not by a second proof. Browsers
  cap how large a single disk-cache entry they will store, so an 831 MB
  response may simply never be cached client-side at the complete-mainnet
  scale, and the `304` path may only pay off for smaller deployments. It
  costs nothing either way, so it stays.
- **`wire::encode_setup` now pre-sizes its buffer.** At ~277 MB per segment,
  the doubling-growth reallocations `Vec::new()` + repeated
  `extend_from_slice` used to pay would copy hundreds of MB and transiently
  hold two buffers alive at once. The exact final length is now computed up
  front from `bundle.backend_params` alone (never `bundle.hints`, never a
  hardcoded constant), with a `debug_assert_eq!` against the length actually
  written — which doubles as an invariant check that every hint's real
  length still matches `lwe_dim * reshape_row_width`. This is paid at most
  once per cache regeneration now, rather than once per request.

Bandwidth exhaustion from many legitimate-*looking* `/setup` fetches is still
a real concern for a public deployment; that defense belongs entirely to the
reverse proxy already in front of this server (Caddy, `docs/deploy.md` §3.7)
or a CDN (roadmap C3/C5) — layers that can meter bytes actually on the wire,
which is the one thing an in-process concurrency limiter on a
fully-materialized-body handler could never do.
### ADR-0029 — A stalled rewind client re-bootstraps itself, exactly once **[NEW]**

**Chosen:** when `GET /sync` reports the client's `pending_head` has aged out of
the server's delta ring, `PrivateEth::get_balance` re-fetches `GET /mode` **and**
`GET /setup`, replaces the whole session, and retries the lookup **once**. A
second `Stalled` is returned to the caller, with a message that says restarting
is what helps.
**Rejected:** leaving the wedge in place and only fixing the wording; and
retrying in a loop.

**Why:** `sync_to` mapped an aged-out range to `Stalled` without touching
`pending_head`, so every subsequent call re-requested the same dead range — the
process was wedged permanently, and the JSON-RPC message said `try again`, which
was the one thing that could never work. The contract held (it failed loudly
rather than answering against a mismatched epoch), but this is a liveness gap
with actively misleading guidance. It is not an idle-client curiosity either: a
server replaying a catch-up backlog advances ~50 blocks/min against ~5 in steady
state and simply outruns the ring, so a freshly started client hits it too — as
the co-located `:8545` front end did on 2026-07-26 (`docs/deploy.md` §5.3).

A re-bootstrap introduces no new trust or correctness surface: it is byte-for-byte
what a freshly started process does. What makes it *safe* is that it is
all-or-nothing. Everything derived from the deployment now lives in one
`Session` behind the existing mutex — the `RisePirClient`, `pending_head`, the
geometry (`arity`, `plaintext_bits`, `reshape_row_width_per_seg`) and
`strict_not_found` — and `rebootstrap` replaces the struct in a single
assignment. A partially-updated session is unrepresentable, so an old hint can
never be paired with new deltas.

**`GET /mode` is re-fetched, not carried over, and that is a binding-rule
matter.** If a deployment were restarted from complete to partial, a client that
kept `strict_not_found = false` would answer `0x0` for an account that is merely
untracked — a silently wrong balance, which this project calls total failure
(ADR-0015/0017). Re-reading `/mode` costs one small request on a path that is
already downloading the hint.

**One retry, never a loop.** Against a server replaying faster than a client can
bootstrap, a loop would spin forever while re-downloading the hint each time —
830.73 MB on the live deployment. Bounded retry turns a permanent wedge into a
self-healing common case and an honest error in the pathological one.

**Cost:** the re-bootstrap pays a full `/setup` download, so the first query
after a stall is as slow as a cold start. `pending_head` is never carried across
a re-bootstrap; it is exactly the new bundle's block, never a guess.

**Not covered:** the browser front end, which already told the truth ("Reload the
page to fetch a fresh hint") and re-bootstraps by reload. Doing the same
automatically in the page would mean re-downloading the hint inside a tab, and is
left for whoever revisits `web/`.

### ADR-0030 — Geometry quantization: arity is not the lever, `bucket_size` is — capped at 4 today **[NEW]**

**Chosen:** keep the deployed `arity 3, bucket_size 4` unchanged, and add `xtask
geometry` (`cargo run -p xtask --release -- geometry [--fill-check]`) — an
`arity x bucket_size` sweep over `risepir_proto::geometry::Geometry`, plus an
opt-in fill-check that builds the real `segmented_cuckoo` store — so this
question has a command to answer it, not a one-off spreadsheet.
**Rejected:** switching the deployed geometry to `arity 4, bucket_size 4`, which
a task brief proposed to shrink the 35.43 GB database to 23.62 GB (~12 GB
saved). The arithmetic behind that number is correct — `xtask geometry`'s own
`4/4` row reproduces it exactly, 23,622,320,128 B DB and 784,502,400 B hint —
but the conclusion, that arity is the reason, is not.

**Why — two corrections:**
1. **The database size is a function of load factor, not arity.**
   `Sizes::server_db = num_buckets * bucket_size * cells_per_slot * 4` — `arity`
   is not a term in it. `Geometry::for_accounts`'s own `num_buckets` rule for
   arity 2 and arity 4 is bit-identical (`buckets_needed.max(arity).next_power_of_two()`;
   the `.max(arity)` floor only bites at toy scales), so at 200,503,969 accounts
   every swept `bucket_size` (1–16) gives arity-2 and arity-4 configurations with
   the same `num_buckets`, the same load factor, and the same server DB —
   confirmed twice: arithmetically (`xtask geometry`'s table, any `bs` column)
   and with a real store (`--fill-check`'s `(2,4)` and `(4,4)` rows both land on
   `load 0.5625` at 9,437,184 accounts, zero `TableFull`). `bucket_size` alone
   reaches every load factor a higher arity would have bought: `(3,3)` and
   `(3,6)` both land at 26.58 GB / load 0.6639 with no arity change at all.
2. **Arity moves the hint the wrong way.** `hint_total = arity * lwe_dim * C *
   4`, `C ~ sqrt(db_cells/arity)`, so `hint_total ~ 4 * lwe_dim *
   sqrt(arity * db_cells)` — proportional to `sqrt(arity)`. At the identical
   301,989,888-slot database (both 26.58 GB), `(3,6)` and `(4,9)` give hints of
   721.00 MB and 832.08 MB — arity 4's hint is *larger*
   (`xtask::geometry::tests::sqrt_arity_hint_law` pins the ratio to `sqrt(4/3)`
   within 1%), working directly against the separate effort to shrink the
   browser client's 831 MB first download.

**`bucket_size` is the real lever — a one-line change (`const BUCKET_SIZE` in
`mainnet.rs`) against arity's own `Segmented3aryScheme`/`Segmented3aryCuckooKVStore`
types, threaded through 18 files — but only within `1..=4` today.**
`segmented_cuckoo::SUPPORTED_BUCKET_SIZES` is `1..=4`, hard-enforced by every
arity's store constructor (`validate_common_params`), so the sweep's own
`bucket_size` 5–16 rows are arithmetic-only. `--fill-check` demonstrates the
boundary directly, at 9,437,184 accounts (fits 16 GB), rather than asserting it:

| candidate | requested | inserted | failed | load | elapsed |
|---|---:|---:|---:|---:|---:|
| (3,4) — deployed | 9,437,184 | 9,437,184 | 0 | 0.7500 | 10.2 s |
| (3,6) | 9,437,184 | — | — | — | **construction failed**: `invalid parameters: bucket_size must be in 1..=4, got 6` |
| (3,3) | 9,437,184 | 9,437,184 | 0 | 0.5000 | 10.6 s |
| (2,4) | 9,437,184 | 9,437,184 | 0 | 0.5625 | 15.7 s |
| (4,4) | 9,437,184 | 9,437,184 | 0 | 0.5625 | 17.7 s |

`(3,6)` fails at *construction*, not `TableFull` — it was never an option with
the pinned IKPIR rev, upstream change or not. Every candidate that *did*
construct filled with **zero insert failures**, `(3,4)` landing on exactly
`load 0.7500` — the arithmetic sizing is not just plausible, it is what real
cuckoo eviction achieves.

**The cliff.** Any 23.62 GB configuration (`(2,4)`, `(4,4)`, …) sits at load
0.7469: 201,326,592 accounts fit; the live set is 200,503,969, i.e.
**822,623 accounts of headroom (0.41%)**. One more account forces the next
doubling, to **47.24 GB** — 33% *worse* than today's 35.43 GB. Mainnet's
nonzero-balance set grows continuously; 0.41% is weeks, not years.

**Recommendation — an operator trade, not a change this PR performs:** today's
`(3,4)` buys 50.6% growth headroom at 35.43 GB. `(3,3)` — real and buildable
today, unlike the brief's own `(3,6)` suggestion — buys 13.0% headroom at
26.58 GB / 719.99 MB hint (marginally cheaper than `(3,6)`'s arithmetic-only
721.00 MB, for no upstream dependency). **This PR deliberately does not change
`BUCKET_SIZE`**: the deployed geometry only changes via a full re-bootstrap
(~33 min at the complete set, `docs/deploy.md`), so the trade is the
operator's call — the tool exists to make it an informed one, not to make it
for them.

**The sweep flags what cannot be built, in two different senses.** A row can
fail for two unrelated reasons and the table distinguishes them. `†` is a
`bucket_size` outside `SUPPORTED_BUCKET_SIZES` — arithmetic-only, no store
constructor accepts it. `‡` is a sizing that lands *above* what a real cuckoo
table at that `(arity, bucket_size)` holds: the geometry is well-formed and the
store would construct, but the fill ends in `TableFull` partway through. Each
row now carries its own `maxload` ceiling
(`segmented_cuckoo::MAX_LOAD_FACTOR`) next to the load it was sized to, so the
margin is visible rather than implied.

At the live account count exactly one configuration earned `‡` when this
column was added — `arity 2, bucket_size 1`, sized to 0.7469 against a 0.48
ceiling. That was not a quirk of the tool but a defect in
`Geometry::for_accounts`, which sized every configuration against one flat
0.75 target regardless of arity or bucket size; measured against a real store
it died after 70.1% of its inserts. Surfacing it here is what this column was
for, and **ADR-0031 then fixed the sizing itself**, so no configuration earns
`‡` at the live account count today — `(2,1)` is now sized to `0.85 × 0.48 =
0.408` and fills. The column stays: it is what would make the *next* such
sizing defect visible on the way in, rather than at a `TableFull` mid-block.

**Filling at the load factor that actually matters.** The 9,437,184-account
run above proves each candidate *constructs and fills*, but rounds each one to
its own load — `(3,3)` lands at 0.5000 there, not the 0.6639 the
recommendation turns on. Choosing the account count so the quantization lands
where it does at 200 M fixes that: at **6,265,000 accounts** every candidate
reproduces its complete-set load factor to four decimal places, in miniature,
on this laptop:

| candidate | load here | load at 200,503,969 | inserted | failed | elapsed |
|---|---:|---:|---:|---:|---:|
| (3,4) — deployed | 0.4979 | 0.4980 | 6,265,000 | **0** | 5.5 s |
| (3,3) — recommended | **0.6639** | **0.6639** | 6,265,000 | **0** | 5.4 s |
| (2,4) — cliff edge | 0.7468 | 0.7469 | 6,265,000 | **0** | 5.5 s |
| (4,4) — cliff edge | 0.7468 | 0.7469 | 6,265,000 | **0** | 5.7 s |
| (3,6) | — | 0.6639 | — | — | **construction failed** (`bucket_size must be in 1..=4, got 6`) |

So the operating point `(3,3)` is proposed at is not an extrapolation: real
cuckoo eviction fills that exact load with zero insertion failures. Upstream's
own measured tolerance agrees with a wide margin — `segmented_cuckoo`'s
`MAX_LOAD_FACTOR` table gives **0.94** for `arity 3, bucket_size 3` (and 0.94
for `(3,4)`), against the 0.6639 proposed here and the 0.75 target this repo
sizes to. Load factor is not what constrains this geometry; the factor-of-two
quantization of `num_buckets` is.

**Measured vs. computed.** The sweep table is exact, closed-form arithmetic
from the repo's own `Geometry` — `cargo test -p xtask` pins five of its
rows/invariants against the figures above. The fill-check is real (a real
`segmented_cuckoo::CuckooKVStore`, real inserts) but at 9,437,184 and
6,265,000 accounts; the live 200,503,969-account scale needs the 64 GB
production box and was not attempted here. What transfers from those runs to
200 M is the *load factor*, which is reproduced exactly, not the wall-clock,
which is not. Nothing above is stated as measured unless it was.

**Follow-up:** expose `--bucket-size` as a flag on the mainnet bootstrap path
(today changing it means a recompile); upstream in IKPIR, non-power-of-two
`segmented-cuckoo` segment sizes — the masking-based hash in that crate is
both why `num_buckets` quantizes by factors of 2 and why
`SUPPORTED_BUCKET_SIZES` hard-caps `bucket_size` at 4. `docs/HANDOFF.md`'s
upstream-candidates bullet now points here.
### ADR-0031 — `for_accounts`'s target load is `min(0.75, 0.85 × segmented_cuckoo::MAX_LOAD_FACTOR)`, not a flat 0.75 **[NEW]**

**Chosen:** `Geometry::for_accounts` now sizes each `(arity, bucket_size)`
against `target = min(GLOBAL_TARGET, SAFETY_MARGIN × MAX_LOAD_FACTOR[arity-2]
[bucket_size-1])`, with `GLOBAL_TARGET = 0.75` (unchanged) and `SAFETY_MARGIN =
0.85`, reading the ceiling from the real `segmented_cuckoo::MAX_LOAD_FACTOR`
(now a normal dependency of `risepir-proto`) rather than a copy of it, so the
rule cannot drift from the primitive it sizes. The search stays exact `u128`
arithmetic end to end — the published `f64` ceilings are converted to exact
integer hundredths once, and both candidate targets are compared by
cross-multiplication, never as floats.

**Rejected:** upstream's own `target_load_factor` un-margined (`0.91`–`0.95`,
what `CuckooKVStore::from_num_items` sizes to directly) — it is calibrated for
a fill-once benchmark, and this store mutates continuously: inserts land
inside a ~12 s block budget while holding the write lock `/answer` also
needs, cuckoo eviction chains lengthen sharply as load approaches the
ceiling, and the store cannot grow in place once built
(`RisePirServer::full_rebuild` only ever re-derives hints for the *existing*
geometry — see that method's docs). A `TableFull` mid-block is therefore a
full re-bootstrap outage — ~33 min of CPU at the deployed scale, plus the
catch-up replay — not a slowdown, so the flat 0.75 headroom stays as an
outer bound regardless of what any one configuration could theoretically
reach. Also rejected: rejecting `bucket_size` outside `1..=4` outright (no
`MAX_LOAD_FACTOR` entry exists there) — a sweep tool on another branch
deliberately sizes `bucket_size` 5..16 for arithmetic-only exploration, so
`for_accounts` falls back to the flat 0.75 for those instead of refusing
them.

**Why:** every configuration was being sized against one flat 0.75 with no
regard for arity or bucket_size, and `segmented_cuckoo`'s own achievable load
is not flat — `MAX_LOAD_FACTOR[arity-2][bucket_size-1]` ranges from `0.48`
(`arity=2, bucket_size=1`) to `0.95` (`arity=4, bucket_size=3` or `4`). At the
low end, `0.75` sits *above* the achievable ceiling, not below it: measured
directly, `Geometry::for_accounts(1_500_000, 2, 1, …)` sized to 2,097,152
buckets (load 0.7153), and a real `Segmented2aryCuckooKVStore` built at that
geometry hit `table is full` after only 1,051,458 of 1,500,000 inserts
(70.1%) — `for_accounts` had returned a geometry the store could not actually
be filled to, with no error and no warning. Upstream already publishes
exactly the table this needed (`segmented_cuckoo::MAX_LOAD_FACTOR`) and a
`from_num_items` constructor that sizes from it; this repo used neither, and
had been sizing a strictly worse, uniform stand-in for the same idea.

At margin 0.85, the per-configuration term binds on exactly three
configurations — `(2,1)` → `0.408`, `(2,2)` → `0.7055`, `(3,1)` → `0.7225` —
and every other `(arity, bucket_size)` in `2..=4 × 1..=4` still resolves to
*exactly* `0.75`, asserted in tests rather than trusted. None of the three is
used anywhere in this repo — every deployed and benched configuration is
`(arity=3, bucket_size=4)` — so this is a no-behavior-change everywhere this
repo actually runs, and a correctness fix everywhere else.

Deflating but honest: at the deployed complete set (200,503,969 accounts,
`arity=3, bucket_size=4`) this changes **nothing** — `num_buckets` stays
100,663,296, load 0.4980, bit-identical to before. `num_buckets` is so
coarsely quantized (a power of two per segment) at this scale that raising
the target all the way from 0.75 to `(3,4)`'s own ceiling of 0.94 still lands
on the same 100,663,296 — the deployed load would need to very nearly double
before the quantization step even notices a higher target. So this is a
correctness fix for the edges of the `(arity, bucket_size)` space and for
account counts that quantize differently, not a space win for the set
actually running today.

### ADR-0032 — Browser pre-flight: measure the real cost per deployment, advise rather than block **[NEW]**

**Chosen:** before `boot()` calls `connect()`, `app.js` issues a `HEAD
/setup` and reads `Content-Length` — this deployment's exact hint size,
paid for with headers only, never guessed — and estimates the peak
resident cost at 2x that figure (hint plus the publicly-seeded matrix `A`,
which the client expands locally to very nearly the hint's own size;
docs/numbers.md §4c measures that ratio at ~2.00–2.03x across every
deployment scale in its table, e.g. the live complete mainnet set: 830.73
MB hint, 1.66 GB resident). It compares the estimate against half of
whatever `navigator.deviceMemory` reports (`USABLE_MEMORY_FRACTION =
0.5`, `web/pir.js`) and, only when the estimate clearly exceeds that
budget, swaps the "01 One-time setup" panel for an explanation — the
download size, the estimated resident cost, what the device reported, and
a pointer at the CLI client (`risepir-rpc client --pir-url …`, which puts
the identical rewind client outside the browser without changing the
"address never leaves the machine" guarantee) — plus an explicit "Download
anyway" that proceeds regardless. Where `deviceMemory` is unavailable
(Safari, Firefox — it is a Chromium/Edge-only API), the pre-flight never
refuses; at most it shows the same panel, softened, when a coarse-pointer
or small-viewport signal suggests a phone or tablet *and* the estimate is
large, and even then nothing is blocked, only flagged.

**Rejected:** (a) hard-blocking on a `navigator.userAgent` sniff — a string
match is trivially wrong in both directions (a spoofed UA, a legitimate
desktop with an unusual UA, a phone with generous RAM) and gives the
person being turned away no real numbers, where `deviceMemory` and a
measured hint size are actual quantities the page can show; (b)
hardcoding the 831 MB figure — the hint size is a property of *this*
deployment (`mock` ships ~1.77 MB, `--partial-capacity 1000000` ships ~49
MB, the complete mainnet set ships 830.73 MB), and a constant threshold
would either fire on every demo or silently stop protecting anyone once
the account count grows past whatever got typed in; (c) doing nothing —
the status quo is not "unavailable at the complete set", it is `boot()`
calling `connect()` unconditionally, downloading hundreds of megabytes on
a device that cannot expand them, and dying mid-expansion with no
explanation at all, which is exactly the silently-wrong-outcome shape
this project's binding rules exist to close off (CLAUDE.md: "when in
doubt, fail loudly" — here, failing loudly means saying so *before*
spending the download, not after).

**Why advise instead of refuse:** a false refusal on a device that could
have handled the download is worse than the status quo it replaces — the
CLI client already runs this deployment comfortably, so a wrongly-turned-away
browser visitor is worse off than one simply allowed to try and, rarely,
fail. `deviceMemory` itself is Chromium/Edge-only, so treating its absence
as grounds to refuse would mean refusing by browser identity, not by any
measured property of the device — the same UA-sniffing failure mode
rejected above, reached from the other direction. `USABLE_MEMORY_FRACTION`
is chosen with this in mind: Chrome/Edge cap `deviceMemory` at 8 regardless
of real installed RAM, and 8 * 0.5 = 4 GB still clears the complete set's
1.66 GB estimate with room to spare, so the fraction itself is never the
reason a capable desktop is turned away. The pre-flight therefore only
ever refuses against a real number that clearly does not fit, and even
then "refuse" is one click from proceeding.

**Scope: pre-flight only.** `web/pir.js`'s `connect`/`PirSession` methods,
the four-step lookup, and every never-a-wrong-answer path (`UNTRACKED`,
`DECODE_FAILED`, the strict-partial rule) are untouched — this decision
runs *before* any protocol call, never inside one. A `warn`/`refuse`
verdict changes what the page shows; it never changes what it answers.

**Where the decision lives.** `assessCapacity` is a pure function — no
DOM, no `navigator`, no `fetch` — precisely so `web/test/e2e.mjs` can call
it directly under plain Node. It would ordinarily be its own file
(`web/capacity.js`, mirroring the existing app.js/pir.js split), and a
small pure decision like this is exactly what that separation is for —
but every file the browser can fetch has to be named in
`crates/risepir-http/src/web.rs`'s fixed asset `MANIFEST`, a deliberately
closed list with no directory-serving fallback to ride a new file in on
(ADR-0019: "no request-path-to-filesystem-path translation, ever"). This
work's brief rules out touching any Rust crate to add one more route, so
`assessCapacity` is hosted in `web/pir.js` instead: already served,
already free of DOM/navigator/fetch, already imported by both `app.js`
and the test file. The decision itself did not change; only which
already-served file it lives in did.

**A no-op on `mock` by construction, not by a special case.** The gate
compares an estimated *byte count* against a budget; it has no notion of
"mock". `mock`'s ~1.77 MB hint estimates to ~3.5 MB resident, which clears
essentially any device's budget (even a 2 GB phone's 1 GB, at
`USABLE_MEMORY_FRACTION`) — so `web/test/browser.mjs`, which only ever
runs against `mock`, exercises the same `boot()` path it always has, with
one cheap `HEAD` round trip ahead of it.
