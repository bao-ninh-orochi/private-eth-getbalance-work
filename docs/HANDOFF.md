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

Six crates, **119 tests green**, all committed & signed:
`risepir-proto` (geometry + codecs, 55), `risepir-server` (batched per-block server +
delete-on-zero, 23), `risepir-client` (response rewind, 10), `risepir-feed` (`Feed` trait
+ seeded mock, 6), `risepir-http` (axum transport + binary wire codec, 25). JSON-RPC,
conformance, and benches are **not built yet**.

The rewind, batching, geometry, the value encoding (task 1), the mock feed (task 2), and
the HTTP transport (task 3, Stage 0.3 — answer/sync/setup/head/delta, exact-length binary
codec, fuzzed no-panic) are all done. **Next is task 4** (JSON-RPC `:8545`, Stage 0.4).

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

**4. JSON-RPC `:8545` (Stage 0.4).** `eth_getBalance` (private, via the client),
`eth_chainId`, `eth_blockNumber` (= our head), `net_version`. **Deny everything else by
default**; `--proxy-upstream <url>` opt-in with a loud warning (ADR-0012). Gate:
`cast balance <addr> --rpc-url http://localhost:8545` returns the right value against the
mock. `"latest"` = our head; document that it lags a real RPC (we follow `finalized`).

**5. Conformance harness (Stage 0.5) — the real gate.** One command, pass/fail: ≥1000
addresses × ≥100 consecutive blocks, byte-identical to in-process ground truth. Sample
must include high-activity, zero-balance, never-existed, **created-during-run**, and
contract accounts. Diff continuously, not once.

**6. Numbers table (Stage 3).** Measure (don't guess): per-block patch time as a **curve
over mutations/block**, per-block delta bytes (naive vs compact codec, on realistic
balances), hint size, query/response bytes, answer latency, client memory, and — the
headline denominator — **full-rebuild time**. Use the `perf/optimized` worktree +
`target-cpu=native`.

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
