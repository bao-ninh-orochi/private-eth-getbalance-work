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
DeltaRing-bridgeability argument; **[REVISED by ADR-0038]** the client-side persistent
cache across browser visits described here as deferred is now built — ADR-0033 supplied
the sharper revalidation this paragraph's own DeltaRing argument was missing, and
ADR-0038 is the persistent IndexedDB cache built on top of it.)

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
  **[REVISED]** Half priced the client's budget at steady state (~5
  blocks/min), but the case that decides the constant is a catch-up replay
  (~50 blocks/min — ADR-0029's own motivating scenario): there, a
  half-window-stale bundle leaves ~6 minutes of window against the 8-minute
  download, so the freshly bootstrapped client stalls *again*, at 831 MB per
  attempt. The window is now an **eighth** of the ring (75 blocks deployed:
  at most ~90 s of staleness even at replay speed, ~10.5 minutes of client
  budget there, ~1 h 45 min at steady state), at the cost of re-encoding at
  most every ~15 min of steady-state chain time (~10 s CPU each at the
  complete set). `NodeState::setup_bytes`'s doc carries the arithmetic;
  ADR-0029's cooldown amendment is the client-side half of the same fix.
- **`ETag` / `304`, honestly caveated.** `GET /setup` sets an `ETag` and
  `Cache-Control: no-cache` (always revalidate, never "don't store"); a
  matching `If-None-Match` gets `304 Not Modified` with no body. Checked
  against the same freshness decision that already gates the `200` path
  rather than a second, separately-maintained check, a `304` is therefore
  only ever returned for a bundle the ring can still bridge forward —
  correct by construction, not by a second proof. **[REVISED by ADR-0033]**
  As shipped here the validator was `"setup-<block>"`, and the by-construction
  argument only holds within one process lifetime: block numbers repeat
  across re-bootstraps, so a block-only validator could `304`-revalidate
  *another lineage's* bundle whenever the heights coincided. ADR-0033 folds
  the lineage epoch into the validator (`"setup-<epoch>-<block>"`), which
  closes that across restarts too. Browsers cap how large a single
  disk-cache entry they will store, so an 831 MB response may simply never
  be cached client-side at the complete-mainnet scale, and the `304` path
  may only pay off for smaller deployments. It costs nothing either way, so
  it stays.
- **`wire::encode_setup` now pre-sizes its buffer.** At ~277 MB per segment,
  the doubling-growth reallocations `Vec::new()` + repeated
  `extend_from_slice` used to pay would copy hundreds of MB and transiently
  hold two buffers alive at once. The exact final length is now computed up
  front by walking the bundle itself (never a hardcoded constant), with a
  `debug_assert_eq!` against the length actually written. The per-segment
  hint contribution is **measured from `hint.data.len()`, deliberately not
  derived from `lwe_dim × reshape_row_width`**: deriving it would quietly
  make the *encoder* a validity check on its input, and it is not one —
  this crate's own decoder tests legitimately encode hand-built bundles
  whose hints do not match their geometry, precisely so the *decoder* can
  be seen to reject them (`setup_encoded_len`'s doc comment records this).
  An earlier revision of this entry claimed the opposite ("from
  `backend_params` alone, never `bundle.hints`"), describing an
  intermediate design that did not survive review — corrected here rather
  than left to contradict the code. This is paid at most once per cache
  regeneration now, rather than once per request.

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
already downloading the hint. **[REVISED by ADR-0033]** As two separate
requests, the re-fetched pair could itself straddle a server restart
(mode from one deployment, bundle from the other — a milliseconds-wide
edge, but the same mixed state in miniature); the mode now rides the
`/setup` response itself (`x-risepir-mode`), so both come from one
response, and the separate `GET /mode` request survives only as a
fallback against servers predating the header.

**One retry, never a loop.** Against a server replaying faster than a client can
bootstrap, a loop would spin forever while re-downloading the hint each time —
830.73 MB on the live deployment. Bounded retry turns a permanent wedge into a
self-healing common case and an honest error in the pathological one.
**[REVISED — a cooldown across calls.]** "One retry per call" still let a
*polling caller* become the loop: every stalled `get_balance` ran its own full
re-bootstrap, 831 MB each. `PrivateEth` now meters re-bootstraps with a
5-minute cooldown (`REBOOTSTRAP_COOLDOWN`): within it, further stalled calls
report `Stalled` without touching `/setup`. The slot is consumed *before* the
attempt (a failed attempt still paid the download), five minutes sits between
the ~8-minute worst-case download (faster retries cannot even finish) and the
~10-minute window a freshly regenerated bundle now guarantees at replay speed
(ADR-0028's eighth-window revision — the server-side half of this same fix).
Pinned by `a_rebootstrap_within_the_cooldown_is_not_paid_again`.

**Cost:** the re-bootstrap pays a full `/setup` download, so the first query
after a stall is as slow as a cold start. `pending_head` is never carried across
a re-bootstrap; it is exactly the new bundle's block, never a guess.

**Not covered:** the browser front end, which already told the truth ("Reload the
page to fetch a fresh hint") and re-bootstraps by reload. Doing the same
automatically in the page would mean re-downloading the hint inside a tab, and is
left for whoever revisits `web/`.

### ADR-0030 — Geometry quantization: arity is not the lever, `bucket_size` is — capped at 4 today **[SUPERSEDED IN PART by ADR-0034 — its `(3,3)` recommendation is dropped; its arity-vs-database-size correction stands, qualified]**

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
### ADR-0031 — `for_accounts`'s target load is `min(0.75, 0.85 × segmented_cuckoo::MAX_LOAD_FACTOR)`, not a flat 0.75 **[RETUNED by ADR-0034 — the constants are now `min(0.90, 0.95 × MAX_LOAD_FACTOR)`; the rule's shape, its exact-integer arithmetic, and its motivating regression are unchanged]**

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

**[REVISED — the multiple is the *init peak*, 3x, not steady state's 2x; and
Save-Data now counts.]** The 2x above was calibrated against §4c's
steady-state `A`+hint figure — but the number that kills a tab is the
*init peak*, and in wasm (whose linear memory never shrinks) the peak is
also the tab's floor forever after. The real sequence peaked near **4x**
the hint: encoded bundle in the input buffer, decoded bundle beside it,
then the client's own hint copy plus the expanded `A` — all
simultaneously live. Concretely: a phone reporting `deviceMemory` 4
(which real 4–8 GB phones do — the API caps at 8 and rounds down) has a
2.0 GB budget, cleared the 1.66 GB estimate, downloaded 830.73 MB on
mobile data, and then had its renderer killed at the real ~3.3 GB peak —
the exact pre-flight failure this ADR exists to prevent, on the most
common phone profile. Two init-sequence fixes cut the true peak first
(`risepir_init` now frees the encoded input buffer between decode and
build; `RisePirClient::from_setup` consumes decoded hints per segment),
landing it near 2.4x, and `ESTIMATED_PEAK_MULTIPLE` is now **3** — that
worst phase rounded up for allocator fragmentation — with its derivation
written next to the constant and pinned by `web/test/e2e.mjs` (estimate
strictly above steady-state resident, at or below the pre-fix 4x, and
`deviceMemory=4` at the complete set now REFUSEs). Separately,
`navigator.connection.saveData === true` — the user's own stated
preference to spend less data — now downgrades an otherwise-`ok` verdict
to the softened panel on any deployment over the coarse-signal threshold
(never to a refusal, and never on demo-scale deployments): memory can be
plentiful and 830 MB still unwanted. And the REFUSE path finally has a
real-browser gate: `web/test/browser.mjs` rigs `deviceMemory` and the
`HEAD /setup` probe inside the page (CDP-injected), then asserts the
gate renders, **no `GET /setup` fires before consent**, and "Download
anyway" boots end-to-end with exactly one `GET /setup` after — closing
the "designed no-op on mock, so nothing ever executed it" blind spot.

### ADR-0033 — Every delta and answer is gated on a hint-lineage epoch; mode rides the setup response **[NEW]**

**Chosen:** derive a 16-hex **lineage epoch** from the setup bundle's
per-segment LWE seeds and reshape dimensions
(`risepir_http::wire::lineage_epoch`, first 8 bytes of keccak256 — computed
identically by the server at `NodeState::new` and by every client from the
bundle it decoded). `GET /sync` and `POST /answer` require the client to echo
it (`?epoch=`) and answer **409** on a missing or mismatched value;
`GET /delta/{block}` requires it in the URL and answers **404** otherwise
(that endpoint is `Cache-Control: immutable`, so its identity has to live in
the URL — `(epoch, block)` never collides across lineages the way bare
`block` does). `GET /setup`'s validator becomes `"setup-<epoch>-<block>"`,
and the response carries `x-risepir-epoch` plus `x-risepir-mode`, so a
client takes the completeness flag and the bundle from **one** response;
`GET /head` carries `x-risepir-epoch` as a cheap change signal. The CLI
(`PrivateEth`) treats an `/answer` 409 exactly like a stalled sync — one
re-bootstrap, then an honest error (ADR-0029); the browser maps any 409 to
`StaleSetupError` ("reload the page").

**Rejected:** a per-process random nonce persisted in the state file (a new
`RPST` field for something the persisted seeds already encode); an epoch
inside the wire bundle body (a format break for every existing client, where
headers and query params are purely additive); trusting the delta-ring
window alone (below); and grandfathering in epoch-less requests (a
"compatibility" hole that would never close — the deployed clients all ship
from this repo, and an old browser tab heals itself with one reload).

**Why the ring window is not enough.** ADR-0019 deferred client-side hint
caching on the argument that "a re-bootstrapped server starts an empty ring,
so a stale-lineage hint gets a 409 rather than a wrong answer". That is true
at the moment of restart and false shortly after: the new process replays
from its snapshot/state toward the head, and once its ring has grown back
over the old client's pinned block `P` (`floor ≤ P ≤ head`, a window that
lasts for the entire time the replay head is within 600 blocks of `P` —
tens of minutes at complete-set replay speed), `GET /sync?from=P&to=head`
is served normally. The layouts of two bootstraps genuinely differ —
`segmented_cuckoo` picks start positions, slots, and eviction victims via
`rand::rng()` (nondeterministic per process), and `server_setup` samples
fresh LWE seeds — so those deltas are meaningless against the old client's
hint: cells shift, the fingerprint scan misses, `Lookup::NotFound` comes
back, and a **complete**-mode client maps that to `0x0`. A silently wrong
balance, reachable by an open browser tab across an operator
re-bootstrap. The same coincidence-of-heights hole existed in ADR-0028's
block-only `ETag` (a `304` blessing another lineage's cached bundle) and in
`POST /answer` (a response decoded against the wrong hint when
`pending_head` happens to equal the answering head). One token closes all
three.

**Why the LWE seeds are the right token.** They are already: random per
bootstrap (`server_setup` samples fresh seeds; there is no injection path —
`docs/verification.md`), persisted in the state file (so a restart from
state — the *same* lineage, whose deltas genuinely are compatible — keeps
its epoch), stable across `full_rebuild` (which re-derives hints for the
same lineage), and present in the wire bundle (clients re-expand `A` from
them). Nothing new is minted, persisted, or trusted: the epoch is a pure
function both sides compute from bytes they already hold, and the reshape
dimensions are folded in so any future geometry re-derivation without
re-seeding still changes it.

**Compatibility.** Purely additive on the wire (headers + query params), no
state-file or bundle-format change; the deployed VM's state file loads
unchanged. On the first redeploy, clients from before this ADR (open browser
tabs; older CLI builds) get one 409 — the browser shows the existing
"reload the page" guidance, the CLI re-bootstraps once and then reports
honestly. That one-time interruption is the entire migration cost, and it
is honest-by-construction: those clients *cannot prove* their lineage, so
refusing them is the correct posture, not a courtesy outage.

**What this unblocks.** Client-side persistent hint caching (ADR-0019's
deferral; `docs/HANDOFF.md`'s top open item) is now sound unconditionally:
a cached bundle revalidates against a lineage-qualified validator, and even
a stale cache that skipped revalidation is caught at its first `/sync` or
`/answer`. Pinned by `crates/risepir-http/tests/epoch.rs` (notably:
a ring-covered range requested with the other lineage's epoch is refused;
a block-only validator never revalidates) and
`crates/risepir-rpc/tests/rebootstrap.rs` (the 409 → re-bootstrap → correct
answer path). **[REVISED by ADR-0038]** That client-side cache is now
built — keyed on exactly the epoch this ADR derives, per the argument
above.

### ADR-0034 — Deploy `(arity 2, bucket_size 4)` at target `min(0.90, 0.95 × MAX_LOAD_FACTOR)`: the top fillable rung, and the cheapest hint on it **[NEW — supersedes ADR-0030's `(3,3)` recommendation; retunes ADR-0031's constants]**

**Chosen:** move the mainnet and demo geometry from `(arity 3, bucket_size 4)` to
**`(arity 2, bucket_size 4)`**, and retune `Geometry::for_accounts`'s target from
`min(0.75, 0.85 × MAX_LOAD_FACTOR)` to **`min(0.90, 0.95 × MAX_LOAD_FACTOR)`**. At
the live complete set (200,503,969 accounts) this lands **load 0.7469**, a
**23.62 GB** server DB, a **553.82 MB** total hint and **1.11 GB** client resident
memory — against today's 0.4980 / 35.43 GB / 830.73 MB / 1.66 GB. Every headline
number improves by a third at once. Also add an **explicit arity check on the
state-file load path**, because this change makes the running deployment's state
file un-loadable by design and that must fail by name, not by luck.

**Rejected:**
- **Staying at `(3,4)`.** It runs at load 0.4980 — 53% of what the structure holds —
  and pays 35.43 GB and an 830.73 MB browser first load for headroom this deployment
  does not need (§3).
- **ADR-0030's `(3,3)`** (26.58 GB, load 0.6639, a one-line change). Strictly
  dominated once the arity change is on the table: `(2,4)` is 2.96 GB smaller,
  166 MB cheaper on the hint, and runs hotter. `(3,3)` remains the right answer for
  anyone unwilling to touch arity.
- **`(4,4)`** — same rung, same 23.62 GB, same 0.7469, `MAX_LOAD_FACTOR` 0.95 rather
  than 0.91, but a **784.50 MB** hint. It buys cuckoo robustness this workload's
  insert rate says it does not need, and charges 231 MB of browser download for it.
- **Any configuration with `MAX_LOAD_FACTOR < 0.90`** — `(2,1)` 0.48, `(2,2)` 0.83,
  `(2,3)` 0.89, `(3,1)` 0.85. A deployment whose point is that the structure runs hot
  has no business on a rung that cannot.

**1. The rung menu is the whole problem — the target load is not a size dial.**

`num_buckets` is `2^t` (arity 2/4) or `3·2^t` (arity 3), so
`slots = num_buckets × bucket_size` can only take the values **`{2^t, 3·2^t, 9·2^t}`**.
At 200,503,969 accounts that menu is:

| slots | load | server DB | reachable by |
|---:|---:|---:|---|
| `3·2²⁶` = 201,326,592 | 0.9959 | 17.72 GB | **unfillable** — above every published ceiling (max 0.95) |
| `2²⁸` = 268,435,456 | **0.7469** | **23.62 GB** | `(2,4)`, `(4,1)`, `(4,2)`, `(4,4)` |
| `9·2²⁵` = 301,989,888 | 0.6639 | 26.58 GB | `(3,3)` only |
| `3·2²⁷` = 402,653,184 | 0.4980 | 35.43 GB | `(3,4)` ← today, `(2,3)`, `(3,1)`, `(3,2)`, `(4,3)` |

There is nothing between 0.7469 and 0.9959, so **0.7469 is the highest load any
buildable configuration reaches at this account count** — a consequence of
`segmented-cuckoo`'s masking-based hash, not of any load-factor analysis. Raising the
sizing target alone therefore changes almost nothing: swept across all twelve
buildable `(arity, bucket_size)` at the live count, moving the target from
`min(0.75, 0.85×MLF)` to `min(0.90, 0.95×MLF)` changes the chosen geometry of
**exactly one** — `(2,2)`, from 268,435,456 to 134,217,728 buckets — and of no
candidate considered here.

Note also that **no arity-3 configuration reaches the top rung**: arity 3's rungs are
`{3·2^t, 9·2^t}`, whose best fillable load is `(3,3)`'s 0.6639. Wanting maximum load
*forces* the arity change. That is the one respect in which ADR-0030's "arity is not
the lever" needs qualifying: arity is not a lever on **database size at a fixed
load**, which is what that ADR measured and which still holds exactly. It *is* a
lever on **which loads are reachable at all**, because it selects the quantization
lattice. Both statements are true; ADR-0030 only needed the first.

**2. On that rung, arity 2 wins — `hint ∝ √arity` (ADR-0030's own law).**

All four members of the 268,435,456-slot rung have identical `server_db`. They do not
have identical hints:

| configuration | MLF | hint total | client resident | query / response |
|---|---:|---:|---:|---:|
| **`(2,4)`** | 0.91 | **553.82 MB** | **1.11 GB** | 435.07 / 434.37 KB |
| `(4,1)` | 0.91 | 784.05 MB | 1.57 GB | 614.62 / 614.94 KB |
| `(4,2)` | 0.94 | 783.60 MB | 1.57 GB | 614.98 / 614.59 KB |
| `(4,4)` | 0.95 | 784.50 MB | 1.57 GB | 614.27 / 615.30 KB |

`xtask::geometry::tests::sqrt_arity_hint_law` already pins the mechanism. Dropping to
two segments is what turns a 12 GB database saving into a **33% cut on the browser's
first download** as well — the constraint `CLAUDE.md` calls "the whole product
constraint" and ADR-0032 built a pre-flight around.

That cut changes a product verdict, not just a number. ADR-0032's pre-flight estimates
the browser's init peak at `3 × hint` and refuses when it exceeds half of
`navigator.deviceMemory`. The device class that pre-flight was written for — real
4–8 GB phones, which report `deviceMemory = 4`, budget 2.0 GB — was refused at
`(3,4)` (peak `3 × 830.73 MB` = 2.49 GB). At `(2,4)` the peak is `3 × 553.82 MB` =
**1.66 GB, inside the 2.0 GB budget**, so those devices are now admitted. The complete
set stops being desktop-only for them. `web/test/e2e.mjs` asserts both halves — the
refusal still fires at the historical `(3,4)` size, so ADR-0032's regression cannot be
retired by a geometry shrink, and the new admission is asserted rather than assumed.

**3. The load is safe here because this workload almost never inserts.**

Measured on the live deployment, bootstrap → first reload (`docs/deploy.md` §5.3):

| | block | accounts |
|---|---:|---:|
| snapshot ingested, setup done | 25,613,233 | 200,503,969 |
| state reloaded | 25,613,849 | 200,510,802 |
| **Δ over 616 blocks** | | **+6,833** |

**≈ 11.1 net new accounts/block**, ~79,850/day at 7,200 blocks/day. Mainnet's
balance-changing rate is ~300 accounts/block (the brief's figure; `docs/verification.md`
Correction 9 measures *Sepolia* at ~140/block and corroborates the shape, it does not
re-measure mainnet). Insertions are therefore **~3.7%** of the write load — ~7.4% even
if the true mainnet rate were Sepolia's ~140 — and the other ~93–96% are updates and
deletes, which `apply_change` routes to `store.update` / `store.delete`, neither of
which walks an eviction chain. Cuckoo insert cost — the thing a high load factor
actually makes expensive — is paid on roughly one mutation in twenty-seven.

This is one 616-block window (~2 h of chain time), not a long-run average, and it is
the only interval for which two exact account counts exist in the log. It is reported
as such. It settles the question it is used for — whether inserts or updates dominate
— by more than an order of magnitude either way.

**4. Why the target constants move too, and why that is not cosmetic.**

`(2,4)` is chosen correctly by the *old* 0.75 target as well. The constants must still
move, for a reason that only bites later: with a flat 0.75, capacity for this geometry
is `0.75 × 268,435,456 = 201,326,592` accounts — **822,623 above today's count, about
ten days of growth**. Past that, any future re-bootstrap silently sizes up to `2²⁹`
slots and **47.24 GB**, worse than the 35.43 GB it replaced. The 0.75 figure was never
a property of the structure; it was headroom policy, and this deployment has
explicitly traded headroom for density.

At `min(0.90, 0.95 × MLF)`, `(2,4)`'s target is `0.95 × 0.91 = 0.8645`, capacity is
232,062,451 accounts, and the runway becomes:

| threshold | accounts | runway at ~79,850/day |
|---|---:|---:|
| target 0.8645 (re-bootstrap due) | 232,062,451 | ~395 days |
| `MAX_LOAD_FACTOR` 0.91 (fill fails) | 244,276,265 | ~548 days |

The blast radius of the retune is the inverse of ADR-0031's: at `(0.90, 0.95)` **ten**
of twelve buildable configurations are bound by the per-configuration ceiling and only
`(4,3)` / `(4,4)` by the flat cap, where before it was three and nine. The
exact-integer arithmetic is unaffected — `95 × 91 = 8_645` and `95 × 95 = 9_025` land
on the existing `TARGET_DEN = 10_000` scale exactly, as `85 × …` did. ADR-0031's
motivating regression still holds: `(2,1)` sizes to `0.95 × 0.48 = 0.456` and fills.
The four pinned `(3,4)` bench/deploy `num_buckets` values (49,152 / 393,216 /
3,145,728 / 100,663,296) are **unchanged** by the retune — verified arithmetically
before the constants moved, so
`for_accounts_deployed_and_bench_num_buckets_unchanged` keeps its meaning rather than
being re-baselined to whatever the new code prints.

**5. What this costs: the demo quantizes worse, and the bench scales flatter it least.**

Arity 2 is not free everywhere. At the partial demo's account counts the `2^t` lattice
lands *badly* where `3·2^t` landed well: at `--partial-capacity 1000000` the geometry
goes 393,216 → 524,288 buckets, so the server DB grows **0.13 → 0.17 GB (+33%)** and
load drops 0.6358 → 0.4768, while the hint improves only slightly, 48.96 → 46.51 MB.
The same shape holds at 250 K / 500 K / 4 M. This is the identical quantization effect
that hands the complete set its win, running in the other direction; the demo is small
enough that paying it is the right trade, but it is a real cost and is recorded here
rather than discovered later.

The same effect dominates `docs/numbers.md` §1–§6, whose three bench scales
(100 K / 1 M / 9,437,184) are all counts where arity 2 quantizes poorly: at 9,437,184
accounts `(3,4)` takes 3,145,728 buckets (load 0.75, 251,658,240 cells) and `(2,4)`
takes 4,194,304 (load 0.5625, **1.33× more cells**). A same-machine control run
confirms the consequence — full rebuild 8.984 s at `(3,4)` against 12.797 s at
`(2,4)`, i.e. `(2,4)` measures 1.42× slower *because it is holding a third more data
at that particular account count*, not because of the arity. **At the complete set the
relationship inverts**: `(2,4)` holds 1.50× *fewer* cells. §7 of that file carries the
control table so the committed §1–§6 cannot be read as "the geometry change made
everything slower". The one genuine arity effect visible in the control runs the other
way: per-block patch time is *lower* at `(2,4)` (5.2321 ms vs 6.8111 ms at the top
scale), because there are two segments to patch instead of three.

**6. The state file does not survive this, and that must fail loudly.**

`state.rs`'s load path reconstructs the store with a **compile-time concrete type** and
performed no arity validation of its own: the format (`RPST2`) carries `CuckooParams`,
and nothing in this repo compared `params.arity()` against the scheme the binary was
built with. After this change the live 36 GB state file — written by a 3-ary binary,
`num_buckets` 100,663,296 — is loaded by a 2-ary one.

The mismatch was *already* caught, and caught by name: at the pinned IKPIR rev
(`3d60fa7`), `CuckooKVStore::<Segmented2aryScheme>::from_cells` checks
`params.scheme_kind` before anything else and returns
`InvalidParams("scheme_kind mismatch: expected Segmented2ary")`. This ADR adds an
explicit check anyway, for three reasons that the upstream one does not cover:

1. **It is not ours.** It lives in a pinned git dependency. Repointing that rev, or an
   upstream refactor of `from_cells`, would remove this repo's only defence against a
   geometry-lineage mismatch, and nothing here would notice.
2. **It reports the wrong class of failure.** It surfaces as
   `StateError::Corrupt("store reconstruction: …")`, and the operator instruction that
   goes with `Corrupt` is "restore from backup or re-bootstrap". The file is not
   corrupt. It is intact and of the previous lineage, and restoring an older backup of
   the same lineage makes things worse, not better.
3. **It fires too late.** It runs during store reconstruction — after `parse_raw` has
   already read and allocated the entire cells array, ~36 GB and minutes of I/O at the
   complete set.

The check therefore sits in `parse_raw`, immediately after the setup header decodes and
before the cells section is read at all, and its message names the cause (a previous
geometry lineage), the fix (move the `--state` file aside and re-bootstrap), and what
*not* to do (restore from backup). The repo's first binding rule — never return a wrong
answer — does not permit relying on a dependency's shape check to stand in for a
geometry-lineage check.

**Operationally this means the change is a re-bootstrap, not a restart.** The running
deployment keeps serving `(3,4)` until the operator moves `~/risepir-state.bin` aside
and re-runs `~/bootstrap-complete.sh` (~33 min at the complete set). Between merge and
that re-bootstrap the code and the live server disagree **by design**; `docs/deploy.md`
§5.3 records which is which, and a plain restart now fails the arity check by name
rather than starting up wrong.

**Measured vs. computed.** The account-growth figures are measured, from the live log,
over the single 616-block window named in §3. The ~300 mutations/block is the brief's
mainnet figure, not re-measured here (see §3). The rebuild/patch/latency figures in §5
are measured, today, on this laptop, both geometries back to back in one session —
absolute times on this machine are ~1.5× slower than the 2026-07-22 run
`docs/numbers.md` §1–§6 previously published, which is why the control was run at all.
Every geometry figure — loads, database sizes, hints, capacities, runways — is exact
closed-form arithmetic from `risepir_proto::geometry`, reproducible with
`cargo run -p xtask --release -- geometry`. `(2,4)` has been demonstrated to fill for
real by `--fill-check`, including at load 0.75 — the deployed operating point — at
reduced scale. No fill has been run at 200 M; that needs the production box.

**Follow-ups.** (a) Arity and `bucket_size` are still compile-time constants behind a
concrete store type; making them runtime flags needs dispatch over the scheme type and
is deliberately not attempted here. (b) The genuinely high-load demo this deployment
wants is `bucket_size = 7` — `(2,7)` at `2²⁵` buckets gives 234,881,024 slots, **load
0.8536** and a **20.67 GB** database — which needs
`segmented_cuckoo::SUPPORTED_BUCKET_SIZES` widened past 4 and a `MAX_LOAD_FACTOR` row
measured for it. That is now the highest-value upstream item in IKPIR for this project,
ahead of non-power-of-two segment counts. (c) `docs/numbers.md` §1–§6 should be
re-measured on a quiet machine, and §7's complete-set rebuild figure re-measured on the
box after the re-bootstrap; both are `(3,4)`-lineage or slow-machine numbers today and
are labelled as such.

### ADR-0035 — Every wait a client makes is bounded, and a stall costs the attempt, not the session **[NEW — completes ADR-0029's "not covered: the browser front end", for stalls]**

**Chosen:** every request `web/pir.js` issues carries an `AbortSignal` from a
stall watchdog — 45 s of *no progress*, re-armed on every chunk received — and a
tripped watchdog surfaces as its own `TimeoutError` (a `PirError` subtype). The
page re-enables its query control in a `finally`, and a timeout renders a retry
that re-runs the *query*. The same two bounds go on the feed's `reqwest` client
(`crates/risepir-feed/src/rpc.rs`): 10 s connect, 60 s read-stall.
**Rejected:** a total per-request deadline (a complete-set `GET /setup` is
553.82 MB and legitimately runs for minutes — any deadline generous enough for
it is no bound at all for `/head`); leaning on the server's own 30 s
`REQUEST_TIMEOUT`, which bounds a *handler*, never a socket that died on the
client's side; and auto-re-bootstrapping the page on a stall the way ADR-0029
does for the CLI, which would charge 553.82 MB for a fault that costs nothing to
retry.

**Why — the browser half.** Reported 2026-07-28: a page left open ~30 minutes
showed `SERVER UNREACHABLE`, and the lookup then span forever without ever
returning or failing. Both halves of the obvious explanation were measured and
are false:

- *The server was fine.* Over 35 minutes against the live deployment: 414
  `GET /head` probes, 0 non-200, max 1.71 s; 344 `POST /answer` probes (valid
  epoch, garbage body — which still takes the state read lock at `node.rs`
  before it decodes, so it measures exactly what a real query waits for), 0
  non-400, max 1.35 s.
- *The 30 minutes was a coincidence, and a good one.* It matches the autosave
  interval exactly, and the reporter's client had last synced at a block the
  server began a 24.18 GB save at. So the save was the prime suspect — and it
  is exonerated: across the 04:10:46 → 04:12:55 UTC save (128.6 s), 25 `/head`
  and 25 `/answer` probes inside the window all answered in 0.7–0.9 s. That is
  ADR-0025's "readers keep flowing for the whole save" confirmed in production
  for the first time, on the real 24 GB write.
- *Staleness alone does not do it.* Driving the same `web/pir.js` and the
  deployed `client.wasm` against the live server: connect, query OK in 5.9 s,
  idle 34 min (160 `/head` polls, 0 failures), query OK in 7.4 s, query again
  OK in 3.7 s — at 237,996 pending delta cells.

What was actually broken was structural, and had nothing to do with elapsed
time. Every request was a bare `fetch()` with no signal, so a socket that
accepts and then goes silent — a laptop sleep, a Wi-Fi change, a network switch,
none of which produce an error — hung forever. `web/app.js` disabled the lookup
button *before* the await and re-enabled it only on paths that require the
promise to **settle**, so one such request disabled the query UI for the life of
the page. The spinner kept turning throughout, because CSS animation runs on the
compositor thread, not the one that is stuck: the page looked busy while being
permanently dead. The "30 minutes" in the report is how long the tab sat before
its first network event after a sleep, not a threshold anywhere in this system.

**Why — the server half, which is the same bug with a worse blast radius.**
`RpcClient` built its `reqwest` client with no timeouts at all, and its own doc
comment explains that it needs none because "the follow loop's own cadence is
the retry: re-asking for the same finalized block is idempotent". That is only
true of a call that *returns*. A half-open socket to the feed — ordinary
behaviour for a keyless public endpoint behind a load balancer — left
`finalized()` or `block_update()` awaiting forever and the follow loop with it.
No `Err`, so no retry; no `critical`, so no halt; no log line at all. The server
would go on answering `/setup` and `/answer` from a frozen head, and the only
outward sign would have been the front end's own "stalled at block N" fifteen
minutes later. The bound turns a silent freeze into an ordinary retry.

**Why these numbers.** Both bounds cap *silence*, never total duration, for the
same reason the Rust PIR client already did (`READ_STALL_TIMEOUT`,
`crates/risepir-http/src/client.rs` — which has had exactly this since it
shipped; the browser and the feed were the two outliers). 45 s in the browser
sits deliberately *above* the server's own 30 s handler bound, so a genuinely
slow answer is reported as the server's honest `408` rather than pre-empted by a
client-side guess. 60 s on the feed, above both, because those are heavy archive
calls (`trace_block`) against a public endpoint — and a false timeout there is
free, since the loop re-asks for the same finalized block.

That leaves one asymmetry, noted rather than quietly fixed: the Rust PIR client
sits at exactly the server's 30 s, so a handler that runs right up to its bound
is a race between the client's stall abort and the server's own `408`. Both
outcomes are loud errors and neither can produce a wrong answer, so this is not
worth a behaviour change to a client that has been in service unmodified — but
it is the reason the two numbers differ, and if that client is ever retuned, 45 s
is the value that makes the server's `408` always win.

**A retry leaks nothing.** Retrying issues a second LWE query for the same
address under a fresh secret. Semantic security makes it indistinguishable from
a query for any other address — which the e2e gate already pins directly
(repeated queries for one address send different ciphertext, at constant size).
The alternative the page used to offer for every failure, "reload and
re-download the hint", is strictly worse on every axis including this one.

**What is pinned, and what is not.** `web/test/e2e.mjs` §8 drives a stalled
socket (a `fetch` stub that settles only via its own `AbortSignal`, which is
what a real dead connection looks like) and asserts the lookup rejects as a
`TimeoutError` within the budget, that the session still answers afterwards, and
that recovering never re-fetched the hint. It is falsifiable and was falsified:
removing the signal makes those checks fail with "still pending after 10x the
budget" rather than passing vacuously. Neither Rust client's timeouts are
unit-tested — a hanging-socket test costs as long as the bound it verifies —
which matches the precedent `client.rs` set. The button's `finally` is
structural rather than tested.

**Still deliberate, and unchanged.** A client more than 600 blocks (~2 h) behind
has genuinely aged out of the delta ring: `/sync` answers `409`, and the page
still says to reload, because there a fresh hint really is the only sound
recovery. ADR-0029's "not covered: the browser front end" is now covered for
stalls; the aged-out case remains a reload by design.

### ADR-0036 — Bound reconcile's request storm during catch-up; defer by lag, backfill from a reservoir **[NEW — extends ADR-0027]**

**Chosen:** five changes to `reconcile` (`crates/risepir-rpc/src/mainnet.rs`):

1. **A per-checkpoint attempt budget**, `samples.saturating_mul(2)` (16 at
   the default `samples = 8`): the sampling loop (`sample_reference`) now
   stops at `checked >= samples` **or** `attempts >= budget`, whichever
   comes first. A checkpoint where every fetch fails now costs at most 16
   requests, not a walk of the whole ~300-address candidate list.
2. **A distinguishable depth-refusal error.** `risepir-feed`'s `FeedError`
   gets a new variant, `DepthRefused` (`Rpc`'s existing fields untouched —
   see **Rejected**). `RpcClient::call` classifies into it from an HTTP
   `403`, or a JSON-RPC error code `-32602`/`-32000` whose message
   contains (case-insensitively) `archive`, `missing trie node`, `state is
   not available`, `state unavailable`, or `pruned`.
   `FeedError::is_depth_refusal()` exposes the answer. Documented as a
   heuristic over third-party error text — see **Why** for the safety
   argument that makes that acceptable.
3. **Defer instead of attempting.** The follow loop already computes
   `finalized` and the block it is applying, so `lag = finalized -
   applied` is free — no new trust dependency. `RECENT_DEPTH_BLOCKS = 64`:
   when `lag` exceeds it, `reconcile` attempts **no fetch at all**.
   `classify_checkpoint` (now taking `lag` as a third argument, checked
   first) returns a new `CheckpointOutcome::Deferred { lag }`, with its
   own log line naming the lag.
4. **A bounded backfill reservoir** (`DeferredReservoir`). Every blind
   (dark or deferred) checkpoint's candidates are queued into a capped
   (`DEFERRED_RESERVOIR_CAP = 256`), deduplicating FIFO. Every checkpoint
   that is *not itself deferred* additionally drains up to
   `RESERVOIR_DRAIN_PER_CHECKPOINT = 2` of the oldest entries, verified at
   *that* checkpoint's own block — a store-vs-provider comparison is valid
   at whatever height it actually runs, independent of which block first
   made the address a candidate. A mismatch found this way halts through
   the same shared path (`compare_one`) as a normal candidate's, naming
   that it came from the reservoir. A drain fetch failure requeues the
   address at the back rather than dropping it.
5. **Truncated per-sample logging** (`FailureLogger`): the first
   `VERBATIM_CAP = 2` fetch failures per checkpoint print verbatim, then
   one trailing `... and {m} more fetch failure(s) this checkpoint` line.
   The dark/deferred/escalation summary lines are untouched.

`GET /healthz` gains three fields on `NodeState`/`ReconcileHealth`:
`reconcile_deferred_total`, `reconcile_reservoir_checks_total`,
`reconcile_reservoir_len` — appended strictly after `reconcile_halted`.

Net effect on the incident that motivated this: ~300 requests and up to
~300 log lines per checkpoint during a catch-up become ≤16 requests and
≤3 log lines, **plus** a reservoir that gives the addresses a pure
"stop trying" policy would have let slip through unverified forever a
second chance once the provider is reachable again.

**Preserved from ADR-0027 — explicitly, since this ADR extends it rather
than replaces it:**

- Only a value mismatch halts the follow loop. A deferred or dark
  checkpoint, however long the streak, never does — `ReconcileOutcome`
  still has exactly the same two cases (`Continued`/`Halted`) and the same
  meaning; `Halted` now also covers a mismatch found while draining the
  reservoir, through the identical code path.
- `CheckpointOutcome::Empty` still leaves `consecutive_dark` untouched:
  `classify_checkpoint`'s new lag check runs *before* the empty check, so
  an empty, non-deferred, non-dark block is exactly as inert as it always
  was.
- `DARK_ESCALATION_THRESHOLD` (20) and its "re-fire every further 20, not
  once" cadence (`should_escalate`) are untouched — `Deferred` simply
  feeds the same `consecutive_dark` counter `Dark` always did, recorded
  through the same `record_reconcile_checkpoint`, logged through the same
  `maybe_escalate` (a straight extraction of ADR-0027's own escalation
  check, now shared instead of duplicated).
- `GET /healthz`'s first line is still byte-for-byte `ok <head-block>`;
  every ADR-0027 field keeps its name, order, and meaning. The three new
  fields are appended strictly after `reconcile_halted` — nothing existing
  was reordered, renamed, or removed.

**Rejected:**

- **Reshaping `FeedError::Rpc` to carry a classification field**, instead
  of adding `DepthRefused`. Checked with `grep` first: every
  `FeedError::Rpc` construction site (the `call` error closure, the
  null-block case in `block_update_on`, `all_failed`'s multi-endpoint
  combiner) lives inside `risepir-feed::rpc` itself, and nothing outside
  that crate ever pattern-matches a `FeedError` variant — every consumer
  (`risepir-rpc` included) only ever calls `Display` on it. A new variant
  touches exactly one of those three sites: the one that actually has an
  HTTP status and a JSON-RPC error object to classify from. The other two
  are synthetic (a shape mismatch, a combined multi-endpoint summary) and
  cannot honestly carry the flag anyway. Additive won on measured churn,
  not just on principle.
- **Halting after a prolonged deferred streak.** The exact mistake
  ADR-0027 already rejected, reachable again through a new door: a large
  `lag` is a fact about the chain and this deployment's own catch-up
  progress, not about the independent provider's honesty. Halting on it
  would still convert a routine catch-up into a self-inflicted outage.
- **Dropping, rather than requeuing, a reservoir address whose drain fetch
  fails.** Would silently shrink the safety net back toward "skip it"
  every time the confirm provider has a bad moment — exactly what this
  ADR's title promises not to do.
- **A single global request budget instead of per-checkpoint.** Unneeded
  complexity: bounding each checkpoint independently already caps the
  worst case (budget × checkpoints/hour) at a small, fixed, easily-stated
  number, with no cross-checkpoint state to reason about.

**Why:** measured on the live box, 2026-07-28: during a catch-up replay,
`reconcile` walked its full candidate list every checkpoint because
`checked` only increments on success and every fetch failed — the
independent provider's keyless tier refuses archive-depth
`eth_getBalance` with `HTTP 403
{"code":-32602,"message":"Archive requests require a personal
token..."}` — producing 154,010 log lines and ~130,000 useless HTTP
requests over 432 checkpoints, none of it evidence of anything wrong with
this deployment (once caught up, every checkpoint succeeded again).
ADR-0027 already made that period *visible* (dark, escalating at 20); this
ADR makes it *cheap*, and gives the addresses it could not check during
that window a second chance instead of leaving them permanently
unverified.

**The safety argument for the Part 2 heuristic:** `looks_like_depth_refusal`
is a status check plus a substring match over another operator's free-text
error message, and it can misclassify. That is acceptable only because
nothing downstream treats its answer as anything but a scheduling/logging
hint: every consumer of `DepthRefused` still handles it as "this sample's
fetch failed", a checkpoint where every attempt fails is dark regardless of
*why* each attempt failed, and an actual value mismatch is only ever found
by comparing balances that *did* fetch successfully — `DepthRefused` never
appears on that path. Worth noting plainly: today `reconcile` does not
actually branch on `is_depth_refusal()` at all — deferral (§3) is keyed
purely on `lag`, decided *before* any fetch is attempted, so there is no
error yet to classify at that decision point. The classifier's practical
effect today is that a per-sample failure log line's own `Display` text
names the reason when it applies; `is_depth_refusal()` is exposed as a
building block for a future caller (e.g. a smarter feed/confirm fallback
choice) rather than wired into a behavior change in this change.

**What this does NOT fix:** the reservoir is a bounded
(`DEFERRED_RESERVOIR_CAP`-address) best-effort backfill, not a second
complete audit log — a catch-up that touches far more than 256 distinct
accounts before the next non-deferred checkpoint leaves the overflow
unverified by this mechanism specifically (an overflowed address touched
*again* later is still an ordinary candidate then). Deferral makes the
ingest path genuinely **unverified** during catch-up, in exactly the sense
ADR-0027 already documented for a dark streak — this ADR does not add
verification depth beyond the sampled-and-eventually-backfilled candidates
it already had; it only removes the wasted-request cost of failing to get
that same non-coverage honestly, and it is the reservoir that pays part of
that back. Full-coverage per-block reconciliation (`docs/roadmap.md`'s A3)
remains the actual next rung, unaddressed here.

### ADR-0037 — `--journal-restore` becomes the default; `--save-interval` widens with it **[NEW — flips ADR-0026's opt-in-behind-a-soak default now that the soak has held]**

**Chosen:** `--journal-restore` now defaults **on**. The flag itself stays
accepted — a no-op, since it is now the default — for scripts that already
pass it and for operators who want to say so explicitly; a new
`--no-journal-restore` is the off switch, restoring ADR-0026's original
report-only behavior (`journal intact: N records to block X`). Both are bare
flags, consuming no value, exactly like their predecessor. `--save-interval`'s
own default is now *coupled* to that setting: **21600 s (6 h)** when restore is
on, **1800 s (30 min, ADR-0025's original value)** when it is off — an explicit
`--save-interval` always wins over either default, in whichever order the two
flags are given. The startup summary and `--help` both print the resolved
value and say whether it was defaulted or explicit, so which of the two
defaults fired is never left to be inferred. A clean startup that finds a
usable journal now also prints its size against the base file's, once, e.g.:

```
risepir-rpc mainnet: journal: 11 record(s), 3942 bytes since the base save (base state file is 24176139523 bytes) — restoring costs the replay, not the rewrite
```

— and the "journal replayed" line now times *only* the replay loop
(`RestoredState::replay_elapsed`), not the base file's own read, so the number
it reports is not inflated by a cost this change does nothing to shrink. Both
`RestoreError` variants `load_with_journal_restore` can fail with now name
`--no-journal-restore` in their own `Display`, so the operator is told the
remedy by the error itself, not left to find it in a doc.

**Rejected:** *leaving the default off and just widening `--save-interval`
manually per deployment* — the whole point is that an operator has to
remember to do that correctly, twice, on every fresh box, and the live
complete-set deployment already proved that "remember to flip a flag later"
does not happen (`--journal-restore` had never once been switched on since
ADR-0026 shipped it). *Decoupling `--save-interval`'s default from
`--journal-restore` entirely* (e.g. defaulting it to 21600 unconditionally) —
would silently reintroduce the unbounded-replay exposure ADR-0025 exists to
bound, for anyone who explicitly opts back out with `--no-journal-restore`;
the two defaults must move together or the flag stops meaning what it says.
*Auto-falling-back to the base save on `RestoreError::ApplyFailure`* instead of
aborting — serving from a base the operator never chose, silently, the moment
replay looks suspect, is a worse failure mode than a loud abort: this repo's
whole posture is that erroring is fine and a silent surprise is not, and an
automatic fallback is still a silent surprise, just a smaller one.

**The problem, measured** (live deployment, 2026-07-28): the complete-set state
file is 24,176,139,523 B (24.18 GB); at the unconditional pre-ADR-0037 default
of 1800 s, the follow loop rewrites all of it roughly every 32 minutes, each
save costing 121–128.6 s at 188–200 MB/s (the exact range §5.4/§5.5 already
measured live: 123.4 s at 196 MB/s, 128.6 s at 188 MB/s). That is ~45 saves/day
≈ **1.1 TB written per day**, entirely to buy a bound on replay that the
journal has been capable of buying — at roughly 1/1000th the bytes — since
ADR-0026 shipped.

**What the soak evidence actually was, and why it now suffices.** ADR-0026
framed `--journal-restore` as deliberately opt-in: *"an operator opts in only
once the report-only scan has shown a healthy journal for a while"* — it did
not have the soak period yet when it was written. That period has now run, and
what it consists of is stated plainly rather than rounded up:

1. **The write path has been unconditionally live since ADR-0026 landed**, and
   the report-only scan it left running — `journal intact: N records to block
   X (--journal-restore to use)` — has come back clean on every restart of the
   live complete-set deployment since its 2026-07-27 re-bootstrap onto
   `(arity 2, bucket_size 4)`; it has never once reported corruption in
   production. That calendar window is short in absolute days, which is
   exactly why the next two points, not elapsed wall-clock time alone, are
   what actually carries this decision.
2. **The failure surface is now exhaustively pinned, not merely exercised.**
   14 unit tests (`journal.rs`) and, after this change, 11 integration tests
   (`tests/journal.rs`, up from 7 at ADR-0026's own writing) cover header
   corruption, base mismatch, torn tails, mid-file bit flips, height gaps,
   oversized lengths (both the absolute cap and the independent
   remaining-file-size bound), the digest-matches-but-block-doesn't
   cross-check, a row-straddling offset, and the apply-time bound violation —
   plus `fuzz/fuzz_targets/journal_scan.rs` against arbitrary bytes, run
   nightly. The specific new one this change adds,
   `recovery_drill_after_a_save_and_a_crash_restores_to_the_last_applied_block`,
   is the in-process rehearsal of exactly the live drill this PR's author runs
   next: save mid-run (not at empty genesis), apply more blocks into the
   journal alone, "crash" (drop down to nothing but the two files on disk),
   reload with restore on, and require the restored head to be the last
   *applied* block, byte-exact against a reference that never restarted at
   all.
3. **The failure mode this soak is standing in for is bounded by design, not
   by trust.** Every way a journal can be wrong degrades to one of exactly two
   shapes, both already true before this change and untouched by it: a
   pre-apply defect (bad checksum, decode error, height gap, torn tail)
   *stops at the last good record* and is never silently skipped past
   (`ScanStop::Invalid`); an apply-time defect (a record that passes every
   pre-apply check but would drive a cell out of bounds) refuses to serve at
   all (`RestoreError::ApplyFailure`) rather than guess which prefix is still
   good. Flipping the *default* changes how often this code path runs in
   production; it changes nothing about what happens when it is wrong. The
   worst case was, and remains, a loud abort — never a wrong balance served.

**Why this is safe even so — the real backstop is elsewhere and untouched.**
The independent-provider reconciliation loop (ADR-0027) is what actually
guards against a *wrong* answer reaching a caller, and it runs on its own
cadence (`--reconcile-every`, default every 30 blocks ≈ 6 minutes of chain
time) — entirely independent of `--save-interval` or `--journal-restore`. A
hypothetical replay divergence subtle enough to pass every structural and
apply-time check in this journal would still have to survive the very next
reconcile checkpoint to go undetected, and a mismatch there halts the follow
loop immediately (serving frozen at the last good block, CRITICAL logged).
Widening `--save-interval` to 6 h therefore does not widen the window in which
a wrong balance could be *served* — it only widens how much journal a restart
might have to replay, which the evidence above puts at seconds, not minutes,
per thousands of blocks.

**Not fixed by this change, stated plainly:** the graceful-shutdown save still
streams and checksums the entire cells array — still ~121–128.6 s at the
complete mainnet set's 24.18 GB, unchanged by anything here — and the base
state file itself still has to be read in full on *every* start, restore on or
off; the journal shortens what has to be *replayed* after an ungraceful kill,
it does not touch what has to be *read*. Both remain exactly the costs
ADR-0025's autosave and this ADR's `--save-interval` widening are bounding
around, not eliminating.

**Evidence.** All pre-existing coverage (ADR-0025's, ADR-0026's) is unchanged
and still passes. New for this change: parser tests for `--journal-restore`'s
new bare-flag default-on behavior, `--no-journal-restore` as its mirror, and
all four `(--journal-restore on/off) × (--save-interval explicit/defaulted)`
combinations, including that flag order does not affect the resolved value
(`crates/risepir-rpc/src/main.rs`); and the integration-level recovery drill
described above (`crates/risepir-rpc/tests/journal.rs`). `cargo clippy
--workspace --all-targets -- -D warnings`, `cargo test --workspace`, and
`RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps` all pass — see
this change's commit for the literal output.

### ADR-0038 — Persist the hint in IndexedDB, keyed by epoch, with server-side `Range`/`If-Range` resume **[NEW — builds ADR-0019's deferred hint caching, unblocked by ADR-0033]**

**Chosen:** the browser client persists the raw `GET /setup` bytes in
IndexedDB, in ~16 MiB chunks, keyed by the hint-lineage epoch
(`wire::lineage_epoch`, ADR-0033) — never the block a bundle happens to be
pinned at. `GET /head` now also carries `x-risepir-mode` (it already carried
`x-risepir-epoch`), so a fresh page load learns the live `(epoch, mode)` pair
for the price of one 8-byte body, cheap enough to pay before deciding whether
a cached entry even applies — far cheaper than a `/setup` request, which can
pay `NodeState::setup_bytes`'s cache-regeneration cost (~10 s CPU at the
complete set). `GET /setup` gained `Accept-Ranges: bytes` and single-range
`Range` support, gated on a matching `If-Range` (below), so a client can
resume an interrupted download — across a page reload *or* mid-session,
after a stall — by asking only for the bytes it does not already have.
`PirSession.load()` now tries, in order: a complete cache hit (no network at
all), a resumed partial download (`Range` from the last committed chunk
boundary), then a plain fresh download — falling back one step further at
the first sign of trouble in each. Every download, of any kind, writes
completed chunks to IndexedDB as it goes, and every `/sync`/`/answer` `409`
(`StaleSetupError`) evicts that epoch's cached entry immediately.

**Rejected:**
- **The Cache API** (`caches.put(request, response)`), the obvious first
  reach for "cache an HTTP response" in a browser. Caching a response whose
  body can run to 553.82 MB needs the platform to buffer the *whole* body
  somewhere before the entry commits — either `response.clone()`/
  `body.tee()` (an unbounded second in-flight copy racing the first) or
  reading the body fully into a `Blob` first. Either shape adds a second
  ~554 MB buffer at exactly the moment ADR-0032's capacity pre-flight is
  budgeting the wasm init peak (`ESTIMATED_PEAK_MULTIPLE`) against the
  device's memory — the number this project already fought hard to keep
  honest (ADR-0032's own regression: a 4 GB-phone estimate that quietly
  missed the true peak). A chunked IndexedDB store writes one ~16 MiB chunk
  at a time — bounded transient memory, no second whole-body buffer — and
  gets resume-across-*sessions* for free, as a side effect of being keyed by
  byte offset at all. Cache Storage also has no notion of resuming a
  half-written entry; IndexedDB's per-chunk records are naturally resumable.
- **Caching keyed by block**, matching `/setup`'s own pre-ADR-0033 `ETag`
  shape. Wrong for the same reason ADR-0033 folded the epoch into that
  `ETag`: `NodeState::setup_bytes` regenerates the cached bundle as the head
  advances (ADR-0028), so two regenerations under the *same* epoch are two
  *different* byte strings at two different block numbers, while a
  re-bootstrap can trivially collide on a block number some earlier lineage
  already used. A block-keyed cache would either evict correct entries
  constantly (treating every regeneration as a new bundle) or, worse, serve
  a stale block's bytes as if they were current. The epoch is stable across
  every regeneration of the *same* bootstrap and changes on every
  re-bootstrap — exactly the granularity a persistent cache needs.
- **A per-download total-duration deadline**, the same rejection ADR-0035
  made for the stall watchdog and for the same reason: a legitimate
  complete-set transfer runs for minutes, so any deadline generous enough
  for it is no bound on a truly dead connection. Resume reuses ADR-0035's
  existing silence-based watchdog rather than inventing a second timeout
  policy.
- **Grandfathering a cache entry across a mode disagreement.** Even though
  mode cannot change within one deployment's process lifetime (`complete` is
  fixed at `NodeState::new`), the live `x-risepir-mode` header is trusted
  over the cached record on principle, not merely by construction — matching
  ADR-0015/0017's "never guessed, never assumed" treatment of the
  completeness flag everywhere else in this codebase. A disagreement evicts
  and re-downloads; it is never resolved by trusting whichever side is
  more convenient.

**Why the epoch is the cache key, and why the server stays the authority.**
Every design question here reduces to the same one ADR-0033 already
answered for the *protocol*: a cached hint is only as trustworthy as the
check that revalidates it, and that check must live on the server, which is
the only party that knows whether a lineage is still bridgeable. This ADR
adds no new trust of its own — it reuses ADR-0033's epoch gate and ADR-0028's
`ETag` machinery for a second purpose (a client-held cache) rather than
inventing a parallel validity story:

- A **complete** cache hit still requires the *live* `x-risepir-mode` (read
  fresh off `GET /head`, never off the cached record) to agree with what was
  cached, and the bundle's own `risepir_epoch()` — derived from the decoded
  hint's LWE seeds, the same way it always has been — is asserted equal to
  the epoch the cache was keyed on as a **hard error, not a warning**, once
  `risepir_init` has run. A cache entry that decodes to a different epoch
  than its own key is corrupt, and ADR-0015/0017's discipline is that a
  corrupt or ambiguous state fails loudly rather than being silently
  patched over — so this evicts the entry and refuses to proceed on it,
  exactly like `risepir-server`'s `FingerprintAmbiguity` refuses a colliding
  store scan rather than guessing which candidate is right.
- A **partial** download's resume is gated on `If-Range` matching the
  server's *current* `ETag` for the reason below.
- Every `/sync`/`/answer` `409` evicts the current epoch's cache entry
  immediately (`PirSession#evictCacheOnStale`) — the same signal that already
  told a pre-cache client "your hint is stale, reload" now also clears the
  thing that would otherwise keep offering that same stale hint back on the
  next boot.

None of this is new trust surface: the operator the page already trusts
(ADR-0019's disclosed code-delivery trust) is the same operator whose
`/head`, `/setup`, and `409` responses this cache defers to. A dishonest
server could already serve a wrong hint before this ADR; it gains no new
way to do so by this cache existing, because the cache never overrides what
the live server says.

**Why `If-Range` is mandatory, not merely honoured.** This is stricter than
RFC 7233 permits — a bare `Range` with no `If-Range` is normally serviced
unconditionally — and that narrowing is deliberate. `NodeState::setup_bytes`
regenerates the cached bundle as the head advances (ADR-0028's eighth-window
rule), and two regenerations under the *same* epoch are two *different* byte
strings: the hint reflects every block patched in up to whichever block it
is pinned at, so byte offset 100 in yesterday's regeneration is not byte
offset 100 in today's. A `Range` request served against the wrong
regeneration would splice two unrelated bundles into a single corrupt hint
— bytes that decode to garbage, which a complete-mode client can surface as
a wrong `0x0`, precisely the failure class ADR-0033's epoch gate exists to
prevent one layer up (and the same hazard ADR-0028's own `ETag` had to fold
the epoch into, for `304`, before this). Requiring `If-Range` to name the
*exact* `ETag` — which already encodes both the epoch and the pinned block —
makes a stale-regeneration range request fail closed onto the ordinary full
`200` instead of a spliced `206`. Proven directly:
`crates/risepir-http/tests/setup_range.rs`'s
`a_stale_if_range_from_before_a_regeneration_never_unlocks_a_206` forces a
regeneration between two requests and asserts the second, stale `If-Range`
gets the full current body, never a `206`.

**What happens on quota refusal or any other cache failure.** Every
IndexedDB call in `web/pir.js` is wrapped so a rejection can never escape
into the boot path — `idbOpen`/`idbGetMeta`/`idbPutMeta`/`idbGetChunk`/
`idbPutChunk`/`idbClearAll`/`idbEvictEpoch` each catch internally and
degrade to `null`/`false`/a no-op, never a thrown exception. A
`QuotaExceededError` mid-write is treated exactly like a browser choosing to
evict a disposable cache under storage pressure: the chunk write simply
fails, downloading continues into the wasm buffer regardless (the hint
still boots this session; only the *next* boot loses the benefit), and no
further chunks are attempted once one write has failed. A missing or
short chunk, a chunk count that disagrees with the recorded total, or a byte
count that does not match `Content-Length` are all treated as "this cache
entry cannot be trusted" and fall through to an ordinary network path —
never a guess at the missing bytes. `indexedDB` being entirely undefined
(plain Node — `web/test/e2e.mjs` runs this file under exactly that — and
some locked-down browser contexts) is the same code path as every other
failure: every `idb*` helper's very first line is `if (!db) return
null/false`, so the whole cache collapses to a no-op with no special-casing
needed at any call site. At most one epoch's data is ever retained — writing
a new epoch's first chunk clears the whole store first — so a tab can never
accumulate an unbounded history of past bootstraps regardless of how many
re-bootstraps it lives through.

**What this does not fix.** The client's resident memory once the hint is
decoded and `A` is expanded is unchanged — **1.11 GB** at the deployed
complete-mainnet geometry (ADR-0034, `docs/numbers.md` §4c) — because that
cost comes from holding the *decoded* bundle plus the expanded matrix in
wasm linear memory, not from re-fetching bytes; a cache hit still pays the
full `risepir_init` decode-and-build sequence and its transient peak, which
is exactly why ADR-0032's capacity pre-flight still runs unconditionally
before every boot, cache hit or not (unchanged by this ADR — see `web/app.js`
and its own comment on the point). The **first-ever visit** to a given
epoch is unchanged too: nothing here shrinks the initial 553.82 MB transfer
itself, only what a *second* visit, a reload, or a resumed interruption
costs. And wasm linear memory never shrinks, so a cache hit does not lower
a tab's floor below what a fresh decode would have set it to — it only
removes the network wait beforehand.

**Measured (`mock`, this repo's own tiny demo deployment, 2026-07-28):** a
second page load — same browser profile, same origin, a `Page.navigate`
reload — booted from the IndexedDB cache in **1022 ms** wall clock
(navigation start to the query box becoming visible; `web/test/browser.mjs`)
with **zero** body-bearing `GET /setup` requests on the wire, against
**1.18 MB** of hint at mock's scale. The in-page boot timer (which starts
*after* the wasm module is already instantiated, so it excludes that fetch)
reported "Ready in 0.0 s". Both numbers are demo-scale, not complete-set
evidence — no state large enough to make a network transfer itself
observable was exercised here — but the wire count is the property that
actually matters and it is exact regardless of scale: a cache hit issues
`GET /head` and nothing else.

**Pinned by:** `crates/risepir-http/tests/setup_range.rs` (`Range`/`If-Range`
handler behaviour, including the regeneration-splice hazard above and a
`HEAD` request behaving identically to `GET` minus the body) and
`crates/risepir-http/src/node.rs`'s own `parse_range` unit tests
(exhaustive: open-ended, explicit bounds, over-long clamping, suffix and
multi-range unsupported, malformed and overflowing integers, `first == 0`
against an empty resource); `web/test/e2e.mjs` (no-`indexedDB` parity under
plain Node, and a stub-fetch test that truncates a `/setup` body then
asserts the retry's `Range`/`If-Range` and a byte-exact functional result
through the resumed session); `web/test/browser.mjs` (a real second
navigation against a real IndexedDB, asserting zero further `GET /setup`
traffic and a correct answer afterward — plus the pre-existing capacity-gate
sub-test, updated to clear IndexedDB before its own "Download anyway" step,
since that sub-test's own question — does consent actually trigger the
network fetch it gates — is now a different question from whether caching
works, and conflating the two would have made a passing cache hit look like
a broken gate).
