# Getting the account-balance data — cheaply, for mainnet

*(Analysis document. The executable steps — gate query, export commands, run —
now live in [`deploy.md`](deploy.md) §2.)*

Answers the question: *can we get a `(address, balance)` key-value database for the
PoC, cheaply/free, near-real-time, without terabytes of storage — and for mainnet?*

**Yes.** The storage was never the problem; acquisition was. Two facts collapse it.

## The two reductions that make this small

1. **Only nonzero balances need storing.** `eth_getBalance` returns `0x0` for a
   nonexistent account *and* for an existing zero-balance account — they are
   indistinguishable on the wire. So the database only holds accounts with **nonzero
   balance**; everything else answers `0x0` correctly *by absence*. This is exact,
   not a compromise, and it eliminates the bounded-universe honesty gap that ADR-0013
   originally had to work around. (Mainnet has ~300M accounts ever seen but far fewer
   with nonzero balance — the exact count is one `SELECT count(*) … WHERE balance>0`
   away and it sets the geometry. Sizing below assumes ~100M.)

2. **Native balance is a tiny slice of state.** Mainnet's ~1.2 TB full node is
   dominated by *contract storage* and *history* — neither of which we need. The
   account set alone (`address → balance`, hash-keyed since we key by
   `keccak256(address)`) is single-digit gigabytes.

Resulting server footprint (from the geometry tool, ~100M nonzero accounts,
3-ary/`bucket_size 4`, 12-byte balance): **PIR DB 8 GB + hint 0.46 GB + A 0.46 GB ≈
9 GB RAM.** Raw `address→balance` is ~4.4 GB. It runs on a laptop.

## The initial snapshot — ranked, with what I actually verified

The steady state (keep current from the block stream, ~300 changes/block) is easy.
The initial complete snapshot is the only hard part. Three paths:

### 1. BigQuery `crypto_ethereum.balances` — "download the answer" ✅ recommended

A public BigQuery table that is *"a snapshot of all account balances, refreshed
regularly, native Ether only"* — literally `address → eth_balance`. Query
`SELECT address, eth_balance FROM \`bigquery-public-data.crypto_ethereum.balances\`
WHERE eth_balance > 0`, export to GCS (free), download (single-digit GB). The table
is small enough that the scan sits inside BigQuery's 1 TB/month free tier.

- **Verified:** the table exists and is documented as a regularly-refreshed native-ETH
  balance snapshot (multiple sources).
- **NOT yet verified — confirm before committing:** its 2026 freshness/lag. The
  community ETL that feeds `crypto_ethereum` (`ethereum-etl-airflow`) was last pushed
  2025-07-06, so the table *may* be stale. If so, the **Google-managed replacement
  `goog_blockchain_ethereum_mainnet_us`** (Blockchain Analytics) is current and has an
  equivalent balance path. **A 5-minute `bq query` settles which to use** — needs a
  GCP account (free tier is enough); I can't run it from here.
- Freshness doesn't have to be at the head: snapshot at whatever block the table
  gives, then replay `B+1..head` from the RPC block stream to catch up.

### 2. Account-only snap download — self-hosted, verifiable, no third party

The `snap` protocol's `GetAccountRange(root, origin, limit, bytes)` returns account
leaves **with Merkle range proofs**, entirely separately from storage. So you can pull
just the ~20–30 GB account trie, skip storage and history, and get a snapshot that is
**cryptographically verifiable against the state root** — a stronger integrity story
than any dataset.

- **Verified:** a standalone tool exists — **Nethereum's `SnapSyncClient`** (14 code
  hits in the Nethereum org), a snap/1 client doing a 16-way partitioned account-range
  walk with a streaming account sink. Hash-keyed output, which is exactly our key space.
- **NOT verified:** whether it completes from public 2026 peers (snap serving is
  best-effort and may be rate-limited), and whether storage fetching can be fully
  disabled. This is the best path *if* BigQuery turns out stale, and the most elegant
  in principle (trustless), but it carries execution risk.

### 3. Xatu `canonical_execution_balance_diffs` replay — ❌ not for the snapshot

Free public parquet, no node — but reconstructing current state means replaying diffs
from genesis. **Measured: 4–18 MB per 1000-block chunk × ~25,500 chunks ≈ 204–383 GB
download** (and recent chunks probed empty, so it may not even reach the head).
Too heavy for a snapshot.

**Keep it for what it is good at:** the **conformance oracle** for a bounded historical
window — one 8–18 MB chunk gives 1000 consecutive blocks of ground-truth
`(address, post-tx balance)`, verified 12/12 exact against archive RPC. That is already
its role in the plan.

## Staying current (the easy part)

Any of: the RPC block stream (`prestateTracer(diffMode) ⊕ block.withdrawals[]`, ~300
changes/block); SQD/Subsquid per-block **state diffs** (cleaner than summing transfers);
or simply re-querying BigQuery on its refresh cadence. All trivial at ~300 rows/block.

## The withdrawal gap — real, but ~30× smaller than feared, and only path-3 has it

Tx-derived sources (Xatu diffs, prestateTracer) **miss beacon withdrawal credits** —
withdrawals credit balances with no transaction (EIP-4895). Two mitigations, both
confirmed:

- The gap is bounded: **~31,896 distinct withdrawal recipient addresses** total
  (894k validators share ~32k addresses because staking pools reuse one). Not ~1M.
- **`canonical_beacon_block_withdrawal` covers mainnet** (2023-04-12 → 2026-07-14,
  daily parquet, ~2.16 MB/day — verified HTTP 200) — the patch source.

**Path 1 (BigQuery balances) and path 2 (snap) sidestep this entirely**: both are
*state snapshots*, not tx-derived, so withdrawals are already reflected. The gap only
bites the diff-replay path and the live RPC stream — for the latter, merge
`block.withdrawals[]` per block, which is free from the block body.

## Recommendation

**Pivot to mainnet** (the user's stated preference, and now well-justified — Sepolia
turned out to be ~150M accounts / 735 GB / sunsetting, buying none of the smallness it
was chosen for). Acquire the initial snapshot from **BigQuery `crypto_ethereum.balances`
if a `bq` freshness check passes, else `goog_blockchain_ethereum_mainnet_us`**; fall
back to the **Nethereum snap account-only download** if the dataset is unusable. Keep
current from the RPC block stream with the withdrawal merge. Store only nonzero
balances. Total server RAM ≈ 9 GB.

**One decision gate before building the ingest:** run the `bq` query to (a) confirm
freshness and (b) get `count(*) WHERE eth_balance > 0` — that number fixes the geometry.
