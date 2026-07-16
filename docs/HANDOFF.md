# Handoff prompt — continue building private-ETH-getBalance

*Paste this into a fresh session to continue. It is self-contained; the linked docs are
the detail.*

---

You are continuing a proof-of-concept **private `eth_getBalance` over RisePIR** (LWE
keyword-PIR). Work in `/Users/admin/Documents/private-ETH-getBalance` (git repo, remote
`bao-ninh-orochi/private-ETH-getBalance`, branch `main`).

**Before writing code, read, in order:** `docs/plan.md` (authoritative spec),
`docs/adr/README.md` (decisions + rationale), `docs/sync.md`, `docs/data-acquisition.md`.
Skim `docs/verification.md` for the measured evidence. The PIR primitive you build on is
at `/Users/admin/Documents/CANS2026/Incremental-Keyword-PIR/crates/` — **read its source
for exact signatures; never guess an API.**

## Where things stand

Seven crates, **135 tests green**, all committed & signed:
`risepir-proto` (55), `risepir-server` (+delete-on-zero, 23), `risepir-client` (10),
`risepir-feed` (seeded mock, 6), `risepir-http` (axum transport + binary wire codec + HTTP
client, 30), `risepir-rpc` (JSON-RPC :8545 + demo binary, 9), `xtask` (conformance gate +
bench, 2).

**Stage 0 AND the Stage 3 numbers table are complete** (tasks 1–6, all committed & signed):
the value encoding, mock feed, HTTP transport, private JSON-RPC `:8545` (`cast balance`
verified), the conformance gate (1201×120, all categories, 0 mismatches), and the measured
numbers table (`xtask bench` → `docs/numbers.md`, full-rebuild-vs-patch headline) are done.

**What's left is real mainnet data (Stage 1)** — gated on the decision the user must run: a
GCP-free-tier `bq` query confirming `crypto_ethereum.balances` freshness + the nonzero count
that fixes the geometry (`docs/data-acquisition.md`), then a `risepir-feed` `rpc` producer,
snapshot ingest, and per-block reconciliation vs archive `eth_getBalance`. Also open: switch
the IKPIR path deps to the pinned git dep `rev = 042d868` (`docs/numbers.md`'s provenance
note explains why this matters for reproducing the §7 headline).

## The binding rules (do not violate)

- **Never return a wrong answer.** Erroring is fine; labelled-stale is fine; a silently
  wrong balance is total failure. Every `NotFound`/`DecodeFailed`/checksum path exists
  for this. When in doubt, fail loudly.
- **Never hardcode `plaintext_bits` or geometry** — derive via `risepir_proto::Geometry`
  (which calls `ikpir_common::pir_params`).
- **Validate every length before allocating** — the server ingests attacker-controlled
  blobs; malformed input must give a clean error, never a panic or OOM.
- **Plan before implementing; use Sonnet subagents to implement** (this driving session
  plans and reviews). Keep the "avoid upstream changes to the IKPIR crates" posture —
  everything so far needs none.
- **Run the tests you write and report real output.** Don't claim green without running.
- Commits: the signing key is in the macOS keychain (`ssh-add --apple-use-keychain` was
  run), so signing works; `commit.gpgsign=true` is global. Push directly to `main` (this
  is the user's private repo — no fork, no PR). Sign every commit.

## Tasks, in order

**1. Value-encoding upgrade — 64-bit-effective fingerprint (ADR-0009). ✅ DONE.**
Shipped: `ValueCodec` is a slot codec (`encode(addr, balance)` → `key_tag ‖ balance ‖
checksum`; `decode(addr, bytes)` → `Lookup{Found,NotFound,DecodeFailed}`, owned by
`risepir-proto`). `key_tag = xxh3_64_with_seed(addr, SEED≠0)` — independent of the SCF's
seed-0 fingerprint, so the two combine to ~2⁻⁶⁰. The client scan masks each slot on `fp`
**and** `key_tag` jointly (see plan.md §4.2 for why fp-alone would be a 2⁻²⁸
silent-wrong-answer). `Geometry::for_accounts` takes the `ValueCodec` and derives
`value_bits`. FP-rate test with a non-vacuity control included. 85 tests green.

**2. `risepir-feed` with a mock (Stage 0.2). ✅ DONE.** `Feed` trait + seeded `MockFeed`
(configurable ~1M keys, ~300 changes/block, realistic wei-scale balances, exact
`balance_of` ground truth). `apply_block` gained delete-on-zero (ADR-0015). An end-to-end
`tests/pipeline.rs` wires mock → `apply_block` → `DeltaRing` → client and diffs private
answers against ground truth across live / high-activity / deleted / never-existed
accounts — deleted keys asserted to read back as exactly `NotFound` (a real removal, and
the first coverage of the rewind handling deletes). 93 tests green.

**3. HTTP transport (Stage 0.3). ✅ DONE.** New `risepir-http` crate: `POST /answer`,
`GET /delta/{block}` (immutable/cacheable), `GET /sync?from=&to=` (coalesced), `GET /setup`,
`GET /head`, all binary over `risepir-proto`'s codec + a SimplePIR-concrete wire codec.
`tokio::RwLock<{server, ring, per_block}>` (ADR-0010). Every decoder pins each segment's
`Vec<u32>` to its **exact** geometry length (a short query would otherwise panic
`server_answer`'s release-mode indexing) and is fuzzed for no-panic/OOM; malformed bodies →
400. End-to-end HTTP test diffs a real client's answers against mock ground truth.

**4. JSON-RPC `:8545` (Stage 0.4). ✅ DONE.** New `risepir-rpc` crate: `eth_getBalance`
(private — keccak256(addr) → `RisePirClient` → HTTP `/answer` → rewind → scan),
`eth_chainId`, `eth_blockNumber` (= our head), `net_version`; everything else denied
(-32601) unless `--proxy-upstream <url>` (loud warning, ADR-0012). `risepir-http` gained an
async `PirHttpClient`. Demo binary seeds known `(address → balance)` accounts (keyed by
`keccak256`) alongside the churning mock. Gate verified: `cast balance <addr> --rpc-url
http://localhost:8545` returns the exact wei value; unseeded → `0x0`; `"latest"` = our head.

**5. Conformance harness (Stage 0.5) — the real gate. ✅ DONE.** `xtask conformance` (new
`crates/xtask`): one command, pass/fail, exit 0/1. Default run diffs 1201 addresses × 120
blocks (floor ≥1000×≥100 also verified) byte-identically against the mock's exact
`balance_of`, across all five categories (high-activity, zero-balance/deleted,
never-existed, created-during-run, contract), diffing at 13 checkpoints (continuous, client
pinned at genesis so every query exercises the full rewind). Any under-sample or empty
category is a loud `HARNESS:` failure, so a vacuous pass is impossible.

**6. Numbers table (Stage 3). ✅ DONE.** `xtask bench` (`crates/xtask/src/bench.rs`) measures
— with `std::time::Instant` against the real built server, at `lwe_dim = 1275` and realistic
wei-scale balances — full-rebuild time (100K/1M/9.4M), the per-block-patch curve over K,
delta compaction (3.35× on realistic balances), answer latency, and computed sizes/client
memory, into [`docs/numbers.md`](numbers.md) (opt-in `--write`). Headline at 9.4M:
rebuild 6.2 s vs patch ~3 ms → ~2100× (perf/optimized `042d868`). Provenance caveat recorded
in `numbers.md` (measured against `042d868`; the local path-dep is on `main`, ~25× slower).

**Then real data (Stage 1):** a `risepir-feed` `rpc` impl (dRPC, keyless: `prestateTracer`
⊕ `block.withdrawals[]`, per `docs/sync.md`), snapshot ingest from BigQuery
`crypto_ethereum.balances` (or `goog_blockchain_ethereum_mainnet_us`), and per-block
reconciliation against archive `eth_getBalance`.

**Decision gate before real data** (needs a GCP free-tier account — ask the user to run
it): `bq query 'SELECT count(*) FROM \`bigquery-public-data.crypto_ethereum.balances\`
WHERE eth_balance > 0'` — confirms the table is fresh in 2026 *and* returns the nonzero
count that fixes the geometry. Until then, Stages 0.x proceed on the mock.

## Known open items to keep in view

- Switch the path deps to the pinned git dep (`rev = 042d868`) before hand-off is final.
- Upstream PR candidates (offer, don't block on): a **batch-mutation API** and **seed
  injection in `server_setup`** (setup is currently non-reproducible). Details in
  `docs/verification.md`.
- Confirm the mainnet nonzero-balance count (sets geometry) — ~100M assumed → ~12 GB
  server RAM.
