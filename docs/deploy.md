# Deploy runbook — private `eth_getBalance` on Ethereum mainnet

The explicit, tested steps to run this PoC, from a 5-minute zero-prerequisite demo
to the complete-mainnet deployment. Everything here uses **free** services; the one
possibly-paid item (the snapshot export's storage/egress) is ≈ $0–1 one-time and is
called out where it occurs.

Every command in §1–§2 was executed as written on 2026-07-19 (see §5 for the
recorded evidence).

---

## 0. What runs where

```
┌─────────────────────────────── your machine / VPS ───────────────────────────────┐
│                                                                                   │
│  risepir-rpc mainnet                                                              │
│  ├── PIR server        :8645   (answer / sync / delta / setup / head — binary)    │
│  ├── JSON-RPC          :8545   (eth_getBalance — PRIVATE; deny-by-default rest)   │
│  └── follow loop       ──────► feed RPC (dRPC, keyless):  traces + blocks         │
│                        ──────► confirm RPC (publicnode, keyless): reconciliation  │
└───────────────────────────────────────────────────────────────────────────────────┘
   wallet / cast ──► :8545      (the server never learns which account you query)
```

- **Feed** (default `https://eth.drpc.org`, keyless, free): serves
  `debug_traceBlockByNumber` + `prestateTracer` — the per-block balance change set.
- **Confirm** (default `https://ethereum-rpc.publicnode.com`, keyless, free): an
  *independent operator* whose `eth_getBalance` the follow loop diffs sampled
  accounts against every `--reconcile-every` blocks. A mismatch halts following
  (serving continues, labelled, at the last good block) — the never-wrong-answer
  backstop against a buggy or lying feed.
- We follow `finalized` (ADR-0007): `"latest"` on this endpoint is ~13 min behind
  the public chain's `"latest"`, by design, and reorg handling does not exist
  because finalized blocks cannot reorg.

## 1. Five-minute demo — `--partial`, zero prerequisites

No snapshot, no accounts anywhere, one laptop. The server starts **empty** at the
current finalized block and tracks exactly the accounts mainnet touches from that
moment on.

```bash
git clone git@github.com:bao-ninh-orochi/private-ETH-getBalance.git
cd private-ETH-getBalance
cargo build --release -p risepir-rpc          # needs access to the pinned IKPIR repo

./target/release/risepir-rpc mainnet --partial
```

Wait for the first finalization burst (up to ~7 min; bursts of ~32 blocks arrive
every ~6.4 min), then:

```bash
cast block-number --rpc-url http://127.0.0.1:8545     # advances in bursts
# pick any tx sender from a recent finalized block, then:
cast balance <that-address> --rpc-url http://127.0.0.1:8545
```

**Honesty rules in partial mode** (all enforced, not advisory):

- An account not yet touched since bootstrap answers a JSON-RPC **error**
  (`-32000 "account not in tracked set…"`), never `0x0` — absence only means zero
  for a *complete* set (ADR-0015/0017).
- Withdrawal credits to untracked recipients are skipped (no true prior to add
  to); those accounts keep erroring until a transaction touches them.
- RAM: ~600 MB at the default `--partial-capacity 4000000`.

## 2. Complete mainnet — snapshot bootstrap

Two phases: a one-time BigQuery export (§2.1 — the only step needing a Google
account), then the server (§2.2).

### 2.1 The snapshot (one-time, ~15 min of clicking + queries)

The source is the public **`bigquery-public-data.crypto_ethereum.balances`** table
(every address's native-ETH balance). You need a GCP project; the **BigQuery
sandbox** (no credit card) can run the *gate* query below free (1 TB/month query
tier — these queries scan a few GB). The *export* needs a GCS bucket, which
requires billing enabled: covered by the new-account $300 credit, or ≈ $0.15
storage + ≈ $0.50–1 egress one-time without it. Delete the bucket after
downloading.

**Gate query** — freshness, the account count, and the snapshot block, in one shot
(`bq` CLI from the Cloud SDK, or paste into the console):

```bash
bq query --use_legacy_sql=false '
SELECT
  (SELECT COUNT(*) FROM `bigquery-public-data.crypto_ethereum.balances`
    WHERE eth_balance > 0)                                   AS nonzero_accounts,
  (SELECT MAX(number) FROM `bigquery-public-data.crypto_ethereum.blocks`
    WHERE DATE(timestamp) < CURRENT_DATE("UTC"))             AS snapshot_block,
  (SELECT MAX(timestamp) FROM `bigquery-public-data.crypto_ethereum.blocks`)
                                                             AS dataset_head_time'
```

- `nonzero_accounts` → `--snapshot-accounts` (sizes the geometry).
- `snapshot_block` → `--snapshot-block`. The dataset refreshes on UTC-day
  boundaries, so the last block of the previous UTC day is the canonical "exact
  at" point. **Check `dataset_head_time` is recent (today)** — if the dataset has
  gone stale (the community ETL stopping is a known risk), stop here and use the
  snap-download fallback (`docs/data-acquisition.md` path 2).
- Getting `snapshot_block` slightly **too low** is safe (re-applied tx changes are
  absolute; see the replay note in §4); too **high** silently misses changes —
  when unsure, subtract a few hundred blocks and let reconciliation prove the
  join.

**Export** (console or CLI; needs a dataset you own for the intermediate table and
a GCS bucket):

```bash
bq mk --dataset yourproject:risepir
bq query --use_legacy_sql=false --destination_table yourproject:risepir.balances '
  SELECT address, CAST(eth_balance AS STRING) AS eth_balance
  FROM `bigquery-public-data.crypto_ethereum.balances`
  WHERE eth_balance > 0'
gcloud storage buckets create gs://yourname-risepir --location=us-central1
bq extract --destination_format CSV --compression GZIP \
  yourproject:risepir.balances 'gs://yourname-risepir/balances-*.csv.gz'
gcloud storage cp 'gs://yourname-risepir/balances-*.csv.gz' ./snapshot/
gcloud storage rm -r gs://yourname-risepir          # stop the meter
```

Result: sharded `balances-000000000000.csv.gz …` files, ~3–5 GB total, rows of
`address,eth_balance` — exactly what `--snapshot` ingests (gzip and the header row
are handled; anything malformed hard-fails with file:line rather than guessing).

### 2.2 Run the server

```bash
./target/release/risepir-rpc mainnet \
    --snapshot 'snapshot/balances-000000000000.csv.gz' \   # repeat --snapshot per shard, in order
    --snapshot-block   <snapshot_block from the gate> \
    --snapshot-accounts <nonzero_accounts from the gate> \
    --state risepir-state.bin
```

What happens, in order (all logged): geometry printout (sanity-check the "server
DB … GB" line against your RAM before it allocates) → streamed ingest (progress
every 5 M accounts) → one-time PIR setup → state saved to `--state` → catch-up
replay `snapshot_block+1 ‥ finalized` through the feed (~1–2 s per block — a
one-day gap is ~2–4 h; the server answers queries the whole time, labelled with
its current block) → steady-state follow.

Restarts are cheap: with `--state`, startup is a file load (bit-identical PIR
parameters — previously bootstrapped clients stay valid) plus the catch-up replay
since the save. `Ctrl-C` saves state before exiting.

### 2.3 Hardware / cost

| deployment | accounts | RAM needed | a $0 way to run it |
|---|---:|---:|---|
| `--partial` demo | ≤4 M tracked | ~1 GB | any laptop |
| complete mainnet | ~100–130 M nonzero | server DB ~10–13 GB + hints/A ~3 GB ⇒ **16 GB floor, 24 GB comfortable** | Oracle Cloud Always Free (4-OCPU/24 GB Ampere A1, when capacity is available) — else a ~€13/mo 16 GB VPS |
| RPC usage | — | — | dRPC + publicnode keyless tiers (the follow loop is ~5–10 requests/min steady-state) |

Run the gate query first — `nonzero_accounts` fixes the real number; the geometry
line the server prints before allocating is the commitment.

## 3. Pointing a wallet at it

- `cast`: `--rpc-url http://127.0.0.1:8545` (only `eth_getBalance`, `eth_chainId`,
  `net_version`, `eth_blockNumber` exist; everything else is denied unless
  `--proxy-upstream` is set, which prints a loud warning about exactly what it
  leaks — ADR-0012).
- MetaMask: add network → RPC URL `http://127.0.0.1:8545`, chain ID 1. It will
  connect and show balances that lag public `"latest"` by ~13 min (finalized).
  Anything the wallet does beyond balance display needs the proxy, with the
  stated leak.
- What the PIR server sees per query: LWE query vectors and which of its own
  endpoints you hit — never the address, and (by LWE hardness) nothing statistical
  about it either. The delta stream is identical bytes for every client.

## 4. Operational notes (the never-wrong-answer contract, operationally)

- **`CRITICAL` in the log** (apply failure, reconcile mismatch, corrupt store):
  following has stopped; serving continues at the last applied block. Fix =
  re-bootstrap from a fresh snapshot (delete/replace the state file). Never
  restart-and-hope into a drifted state.
- **Transient feed errors** (rate limits, truncated bodies) retry the same block
  forever and are routine on keyless tiers; only evidence of *drift* halts.
- **Replay safety**: tx-derived changes are absolute post-state values, so
  re-applying an already-included block is harmless — with one exception:
  withdrawal credits are relative, which is why `--snapshot-block` must not
  *overstate* the snapshot (§2.1) and why reconciliation samples run
  continuously. If a join-time reconcile flags exactly the ~32 k withdrawal
  recipient addresses, the recorded remedy is a one-time absolute refresh of that
  bounded set against an archive RPC (`docs/sync.md`) — not yet automated in this
  PoC.
- **Client fell behind the delta window** (`-32000 "server is resyncing"`): the
  in-process front-end client resyncs from `/setup` on next use; external PIR
  clients do the same.

## 5. Recorded live evidence (2026-07-19, this repo at 64b8f9f)

Partial-mode deployment on a laptop, keyless dRPC + publicnode, real LWE
parameters (`lwe_dim` 1275):

- Bootstrapped empty at finalized block 25,563,526; followed live bursts through
  25,563,546+ (traces + withdrawal credits applied each block).
- **8/8 private `eth_getBalance` queries** for tx-senders from a just-applied
  block returned balances **byte-exact** against publicnode at the same height —
  the full PIR path (query → answer at head → response rewind → joint-mask scan)
  on real mainnet data.
- In-loop reconciliation: `reconcile at block 25563530/25563535/25563540/25563545:
  4 account(s) exact vs independent provider` — every checkpoint exact.
- Strict not-found: an untouched address answered
  `-32000 account not in tracked set…`, never `0x0`.
- `Ctrl-C` wrote a 51 MB state file; restart loaded it in <0.1 s at the same
  block, PARTIAL flag preserved, and resumed following.
- A truncated dRPC response mid-run was retried and recovered automatically.

## 6. Who does what, explicitly

| step | who | needs |
|---|---|---|
| build (`cargo build --release`) | either of us | access to the pinned `bao-ninh-orochi/IKPIR` repo |
| §1 partial demo, end to end | either of us (already done, §5) | nothing |
| §2.1 gate query + export | **you** (Google account; I have no GCP access from this environment) | BigQuery sandbox; billing (or $300 credit) for the export step only |
| §2.2 complete-mainnet run | either of us, on the 16–24 GB box | the shards + the two gate numbers |
| wallet demo | either of us | §1 or §2 running |
