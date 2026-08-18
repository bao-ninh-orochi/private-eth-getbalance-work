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
git clone https://github.com/orochi-network/private-eth-getbalance.git
cd private-eth-getbalance
cargo build --release -p risepir-rpc

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

## 1.5 The browser front end (ADR-0019)

The same rewind client, compiled to WebAssembly and running **in the page**, so a
visitor with a browser gets the same privacy property the CLI client has: the
address is hashed, turned into an LWE query, and rewound locally — the server
sees ciphertext of a fixed size and never learns which account was asked about.

```bash
rustup target add wasm32-unknown-unknown      # once
cargo run -p xtask --release -- web           # builds web/client.wasm (~157 KB)

# mock: complete synthetic set, every address answers, no network
./target/release/risepir-rpc mock --web web
# live mainnet, partial set, 46.51 MB first load (see the table below)
./target/release/risepir-rpc mainnet --partial --partial-capacity 1000000 --web web
```

Then open the **PIR** port in a browser (`http://127.0.0.1:8645/`), not the
JSON-RPC one. Same origin as the PIR transport is deliberate: no CORS, no mixed
content, and the page can be served under a `connect-src 'self'` CSP so it
cannot POST anywhere else.

**First load is the whole product constraint.** The page downloads the PIR hint
once; everything after that is a few KB per query. Pick `--partial-capacity` for
the trade you want:

| `--partial-capacity` | first load | RAM (client) | ~time before the table fills |
|---:|---:|---:|---|
| 250,000 | 23.66 MB | ~47 MB | ~1.5 h |
| 500,000 | 32.64 MB | ~65 MB | ~3 h |
| **1,000,000** | **46.51 MB** | **~93 MB** | **~5 h** |
| 4,000,000 (default) | 93.02 MB | ~186 MB | ~1 day |

(These reflect the deployed `(arity 2, bucket_size 4)` geometry, ADR-0034;
they were 23 / 35 / 49 / 99 MB and ~46 / 70 / 99 / 198 MB at the previous
`(arity 3, bucket_size 4)` geometry. Arity 2 quantizes *worse* than arity 3 at
these small account counts — the same effect that wins big at the complete
set costs a little here: the server's own database grows at every row above
(e.g. 0.13 → 0.17 GB at 1,000,000, +33%), even though the hint a browser
downloads still shrinks slightly. ADR-0034 §5 has the full table.)

Client compute is not the constraint: a full lookup is **10 ms** at 1 M accounts
(two segments, single-threaded wasm, no SIMD), and expanding `A` from its seed
at startup is another ~0.2 s. At the **actual** complete mainnet set —
200,503,969 accounts, measured 2026-07-26, not the ~100 M once assumed here —
the deployed geometry has since moved to `(arity 2, bucket_size 4)` at a
higher target load (ADR-0034): the hint computes to **553.82 MB**, and a
client holds **1.11 GB** resident once `A` is expanded alongside it
(`docs/numbers.md` §4c) — down from **830.73 MB** / **1.66 GB** at the
`(arity 3, bucket_size 4)` geometry the live deployment still runs until an
operator re-bootstraps it (§5.3). That is past what a web page should ask
for, and is where `risepir-rpc client` on a real machine takes over.

**What the page tells the visitor** (all of it enforced, none of it decorative):
the deployment's mode (complete ⇒ absence is exactly `0`; partial ⇒ absence is an
*error*), the block each answer is as of, that `latest` means *finalized* and so
runs ~13 min behind a block explorer, the exact bytes that left the browser, and
— stated on the page, not buried here — that the code delivering all this comes
from the same server, so a user who needs to trust nothing should run the client
themselves.

In partial mode `GET /recent` gives the page up to 128 recently-touched addresses
to offer as examples, since an arbitrary address is (honestly) not in the tracked
set. That list is public chain data, identical for every caller, and fetched
without any address — the server still cannot tell which one, if any, is queried.

### Gates

```bash
node web/test/e2e.mjs     http://127.0.0.1:8645          # protocol, in a real wasm host
node web/test/browser.mjs http://127.0.0.1:8645          # the page, in headless Chromium
node web/test/browser.mjs http://127.0.0.1:8645 --mock   # + the mock's exact seeded values
```

**Both need Node ≥ 22.** There is no `package.json` in this repo (deliberate —
the gates take no dependencies), so older Node treats `web/pir.js` as CommonJS
and the import fails *before any test runs*, with a `SyntaxError: Named export
'PirError' not found` that looks nothing like a version problem. Node ≥ 22
detects module syntax on its own. The deployment VM ships Node 18, so run the
gates from a workstation — which is also the more honest test, since it
measures what a visitor's first load actually costs.

`--mock` asserts the mock deployment's exact seeded balances. Without it the
gate checks the *shape* of the answer and its block label, which is what any
real deployment can promise; `/mode` alone cannot distinguish "the mock" from
"the complete mainnet set", and conflating them made the gate fail against a
perfectly correct mainnet deployment (PR #9).

`e2e.mjs` drives the real wasm through the real `web/pir.js` and asserts, among
other things, that the module's **only** import is the entropy shim (it cannot
phone home) and that two queries for one address ship different ciphertext of
identical length. `browser.mjs` drives Brave/Chrome/Chromium over the DevTools
protocol and checks what only a browser can: that the page boots under its own
CSP, that WebAssembly finds real entropy there, and that an untracked account
renders as an error rather than a `0`. Both adapt to `GET /mode`, so they are
valid against either deployment. Neither needs npm.

### Operational notes

- **Assets are read once, at startup.** Editing `web/*` under a running server
  changes nothing until it is restarted — deliberate (the bytes served cannot
  change under a live deployment, and a missing file is a startup error rather
  than a 404 a visitor discovers), but it will catch you while iterating.
- **`web/client.wasm` is a build artifact**, not checked in. `cargo run -p xtask
  --release -- web` after every pull that touches the client crates.
- **No caching of the hint between visits** — deliberate (ADR-0019): a cached
  hint is only sound while the server still retains the deltas from its pinned
  block, and that argument is not worth standing between a user and a balance
  yet. A reload re-downloads.
- **Not exposed publicly.** `--bind 0.0.0.0` would serve it to the internet over
  plain HTTP, which is the weakest form of the code-delivery caveat above (anyone
  on path can swap the client). Doing it properly means a hostname and a
  certificate — see §3.5/§3.6 for the VM, and ADR-0019 for why serving the page
  from a *different* party than the PIR server is the stronger arrangement.

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
- `snapshot_block` → `--snapshot-block`. **This is not, and was never, an
  "exact at" point** — an earlier revision of this doc claimed the dataset
  "refreshes on UTC-day boundaries, so the last block of the previous UTC day
  is the canonical 'exact at' point", which is measurably false and was the
  root cause of the finding below. Verified directly (`bq show`, not
  inferred): `crypto_ethereum.balances` is a materialized **table**, not a
  view — description *"This table contains Ether balances of all addresses,
  updated daily. Data is exported using
  https://github.com/medvedev1088/ethereum-etl"*, 453,102,032 rows,
  27,186,121,920 bytes, created 2020-01-22. It has **one effective height per
  daily rebuild** — every row in a given rebuild shares one instant, there is
  no per-row versioning and no block-number column to check any of them
  against. `snapshot_block` here is only ever this query's *assumption* that
  that instant equals the last block of the previous UTC day, and **that
  assumption can fail in either direction** — the rebuild may have run before
  or after that block, and nothing in the table itself says which. **Check
  `dataset_head_time` is recent (today)** — if the dataset has gone stale
  (the community ETL stopping is a known risk), stop here and use the
  snap-download fallback (`docs/data-acquisition.md` path 2, the only source
  in this repo that is genuinely, cryptographically atomic at one block via
  Merkle range proofs against the state root — this BigQuery path trades that
  atomicity for being free and requiring no node). The post-bootstrap audit
  (§2.2) is what actually *detects* which way, and by how much, the
  assumption failed for a given bootstrap — nothing else in this pipeline can.
- **Measured, not assumed, ADR-0040 — the export, and, separately, what the
  deployment actually serves.** Re-verifying the 2026-07-25 export against
  the chain at its own declared block found the *export's* error is **not**
  confined to a thin boundary layer: in the 2000 blocks before the declared
  block, 6.9% of touched accounts were wrong (up to 27.99% at depth ≤1,
  decaying to 5.47% at depth (1000,2000]); a population-wide random sample
  with **no** recency constraint still measured **0.33%** wrong (Wilson 95%
  CI [0.09%, 1.21%]) — an implied ~668,000 accounts across the full
  200,503,969. But the export's own error rate is **not** the deployment's
  answer rate: re-running the same population check directly against the
  *live server* (200 addresses, applied head, quorum-verified) found **0
  wrong of 200** (Wilson 95% CI [0.00%, 1.88%]) — the ordinary forward
  replay heals most of what the CSV got wrong, for free. It does not heal all
  of it: re-checking the *specific* rows already known wrong found 28 of 150
  window-wrong rows and 22 of 100 funded-but-absent accounts **still** wrong,
  days later. The two are not in tension — an export error either reflects
  state *after* the declared block (heals unconditionally, the ordinary
  replay reaches it regardless of any rewind) or *before* it (never heals
  from forward replay alone; `--snapshot-rewind` is what reaches backward
  into that window). See ADR-0040 for the full depth table, the live-server
  numbers, and this causal model. Getting `snapshot_block` slightly **too
  low** is therefore not merely "safe" but the *documented default
  behavior*: `--snapshot-rewind` (default 2000, on by default) already
  treats the snapshot as exact that many blocks earlier and lets the
  ordinary replay re-derive the window from the chain's own absolute
  post-state — see §2.2. Getting it **too high** silently misses changes and
  is never safe; when unsure, prefer a lower value and let reconciliation
  and the post-bootstrap audit (§2.2) prove the join.

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

Result: sharded `balances-000000000000.csv.gz …` files, rows of
`address,eth_balance` — exactly what `--snapshot` ingests (gzip and the header row
are handled; anything malformed hard-fails with file:line rather than guessing).

**Recorded run, 2026-07-26** (this is the gate output the live deployment was
built from, not an estimate):

```
nonzero_accounts   200503969
snapshot_block     25613233
dataset_head_time  2026-07-25 23:59:59      # fresh: the previous UTC day's close
```

The intermediate table is 200,503,969 rows / 12.1 GB; the extract is **321
shards totalling 5.64 GiB** gzipped. Pull them straight onto the server rather
than via a laptop — same-region `gcloud storage cp` moved all 321 in **13 s at
669 MiB/s**, and the VM needs only the `devstorage.read_only` scope it already
has. Delete the bucket afterwards to stop the meter.

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

**The export is not exact (ADR-0040) — three flags are part of this procedure,
not optional extras:**

- **`--snapshot-rewind <N>`** (default **2000**, on by default; `0` disables).
  Treats the snapshot as exact `N` blocks before `--snapshot-block` instead of
  exactly at it, so the catch-up replay above re-derives the rewind window
  from the chain's own absolute post-state before it ever reaches the
  declared block. Narrows the *densest* part of the measured error; does not
  close it (see below), and does not fix relative withdrawal credits inside
  the window — `--hard-refresh` does.
- **`--snapshot-audit-samples <N>`** (default **512**, on by default; `0`
  disables). Reservoir-samples that many addresses during the ingest above
  and verifies them against `--refresh-url`'s quorum once setup finishes,
  logging `snapshot audit: N checked, W disagreed … (rate …%, Wilson 95% CI
  […, …])` and writing a `<state>.audit` sidecar so a later restart still
  reports it (also one line on `GET /healthz`). This does not correct
  anything — it measures and discloses whatever residual error remains after
  the rewind above, so a bad export is *visible* instead of assumed away.
- **`--hard-refresh <file>`** (off by default — needs a caller-supplied
  address list). Quorum-verifies each address against `--refresh-url` at the
  current applied head and corrects the store wherever every configured
  provider agrees on a value differing from what is stored. Runs entirely in
  the background (never blocks serving or following) and is idempotent — a
  restart with the same file is a no-op re-verification. This is the tool for
  a *known* suspect list (e.g. accounts the audit above flagged, or ones a
  specific incident turned up); it is not run automatically against the
  whole account set.

None of the three, alone or together, proves the served set is exact — see
ADR-0040 for the full measurement and what remains open. `docs/data-acquisition.md`
path 2 (the account-only `snap` download, verified against the state root via
Merkle range proofs) is the only source in this repo that is genuinely atomic
at one block; this BigQuery path trades that guarantee for being free and
node-free, and the three flags above are the documented mitigation for that
trade, not a claim that it has been eliminated.

Restarts are cheap: with `--state`, startup is a file load (bit-identical PIR
parameters — previously bootstrapped clients stay valid) plus the catch-up
replay since the save — or, with `--journal-restore` (the default since
ADR-0037), since the journal's last replayed record, which is normally much
more recent than the last save. While following, the loop rewrites the file
every `--save-interval` seconds; the default is now coupled to
`--journal-restore` (ADR-0037): **21600** (6 h) with restore on — the default —
since the journal, not the full save, is what bounds replay after an
ungraceful kill; **1800** (30 min, ADR-0025's original value) with restore
off. An explicit `--save-interval` always wins over either default, and the
startup log states which one applied. `Ctrl-C` still saves the exact final
state before exiting; each save logs a `state saved: block …, … GB in …s`
completion line.

Beside `--state` there is always a `<state>.journal` sidecar (ADR-0026, once a
first full save exists): one small per-block delta, appended and fsynced as
each block applies, rotated to a fresh file bound to the new digest right
after every save. It is *written* unconditionally. *Restoring* from it is now
the default (`--journal-restore`, ADR-0037 — ADR-0026 shipped it opt-in,
behind a soak period that has since held): a restart replays the journal onto
the loaded state before serving starts and logs `journal replayed: N block(s)
in T s — resuming at block X (base was B)` (`T` times the replay itself, never
the base file's own read, so it is not inflated by a cost this feature does
not shrink), so a kill -9 between saves costs the journal's replay time (well
under a second per block) instead of a network catch-up. `--no-journal-restore`
opts back out: a restart then only *scans* the journal and logs `journal
intact: N records to block X (drop --no-journal-restore to use it)` — the
original ADR-0026 soak signal, not a decision, still available to anyone who
wants it. Either way, a clean startup that finds a usable journal also prints
its size against the base file's once: `journal: N record(s), B bytes since
the base save (base state file is S bytes) — restoring costs the replay, not
the rewrite`. What used to be the opt-in payoff configuration — a long
`--save-interval` plus `--journal-restore` — is now simply the default:
recovery to within seconds of the last applied block at a small fraction of
the disk-write cost a short interval alone would need.

Journal-writing failures never risk correctness, only durability: a bad
rotation logs a `WARNING` and leaves journaling off until the next successful
save retries it; a continuity gap (should one ever occur) disables journaling
for the rest of that run. Either way the periodic full save keeps happening
on schedule — the worst case is falling back to the previous `--save-interval`
behavior, never a wrong answer at replay time (see ADR-0026's two failure
classes). A journal-*replay* failure is a different, louder case: a record
that passes every structural check but produces an out-of-bounds cell when
actually applied aborts the restart rather than serve a state that cannot be
trusted, and the error names `--no-journal-restore` as the way to start from
the base save alone instead (ADR-0037).

### 2.3 Hardware / cost

**These numbers were revised upward on 2026-07-26**, when the gate query was
first actually run rather than estimated. Mainnet has **200,503,969** nonzero
accounts — not the ~100–130 M this table assumed — which is what actually
drives the geometry. The earlier "16 GB floor" was wrong by more than 2×;
anyone who provisioned from it would have OOMed after paying for a 5.6 GB
download and a 12-minute ingest.

**The geometry itself then changed again, in code, under ADR-0034**: deployed
`(arity 3, bucket_size 4)` moved to **`(arity 2, bucket_size 4)`**, and
`Geometry::for_accounts`'s target load rose from `min(0.75, 0.85×MAX_LOAD_FACTOR)`
to `min(0.90, 0.95×MAX_LOAD_FACTOR)`. At 200,503,969 accounts the new geometry
computes to **67,108,864 buckets, load 0.7469, server DB 23.62 GB** — every
headline number about a third smaller. That is what a *fresh* bootstrap now
produces, not what the live box is currently serving: the running deployment
bootstrapped under the old geometry on 2026-07-26 and keeps serving it
(35.43 GB, load 0.498) until an operator re-bootstraps it — §5.3 records that
original run verbatim, and the state-file loader now refuses to load the old
lineage under the new binary by design (`CLAUDE.md`'s state-file trap
paragraph, ADR-0034 §6).

| deployment | accounts | RAM needed | how to run it |
|---|---:|---:|---|
| `--partial` demo | ≤4 M tracked | ~1 GB | any laptop |
| complete mainnet, `(arity 2, bucket_size 4)` — current code (ADR-0034) | **200.5 M nonzero** (2026-07-26) | server DB **23.62 GB** + hint 0.55 GB + A 0.55 GB ⇒ **~24.7 GB working set** | GCP `e2-highmem-8` (8 vCPU / 64 GB) still comfortably covers it; a smaller box is plausible but unverified — nothing has re-bootstrapped at this geometry at the complete set yet |
| complete mainnet, `(arity 3, bucket_size 4)` — what is live today | **200.5 M nonzero** (2026-07-26) | server DB **35.43 GB** + hints 0.83 GB + A 0.83 GB ⇒ **~38 GB working set: 48 GB floor, 64 GB comfortable** | GCP `e2-highmem-8` (8 vCPU / 64 GB), ~$0.36/h — what the public deployment actually runs on right now |
| RPC usage | — | — | dRPC + publicnode keyless tiers (the follow loop is ~5–10 requests/min steady-state) |

Disk, not just RAM: a state file at the deployed `(2,4)` geometry computes to
**≈24.2 GB** (DB + hints; arithmetic, not yet measured — no re-bootstrap at
this geometry has run at the complete set) against the **36.26 GB** measured
today at the live `(3,4)` file (§5.3). Either way `save` writes `<path>.tmp`
before renaming, so a machine that keeps the previous state file needs disk
for two copies at once — comfortably inside 250 GB at either geometry, plus
5.7 GB of snapshot shards and the build tree.

Run the gate query first — `nonzero_accounts` fixes the real number; the geometry
line the server prints before allocating is the commitment. A fresh bootstrap
on the current code, at its deployed `(2,4)` geometry, now prints:

```
risepir-rpc mainnet: geometry for 200503969 accounts: 67108864 buckets, server DB 23.62 GB, load 0.747
```

(The live box, still running the pre-ADR-0034 binary, printed `100663296
buckets, server DB 35.43 GB, load 0.498` when it actually bootstrapped — §5.3
— and will keep printing that on every restart until it is re-bootstrapped.)

The load factor of 0.7469 is `segmented-cuckoo`'s own quantization, not a
property of the target: `num_buckets` can only take the values `2^t` or
`3·2^t`, so `slots = num_buckets × bucket_size` lands on a short menu of
values regardless of which arity is chosen. At 200,503,969 accounts the
highest rung any buildable configuration can fill is `2^28 = 268,435,456`
slots (ADR-0034 §1) — reachable by `(2,4)` (the deployed choice) and by
`(4,1)`/`(4,2)`/`(4,4)`, since arity 2 and arity 4 share the same `2^t`
lattice — and there is nothing between that and the next, unfillable, rung.
Account growth is therefore **free up to 232,062,451 accounts** — the same
geometry, the same 23.62 GB, at which point load reaches the new
`min(0.90, 0.95×MAX_LOAD_FACTOR) = 0.8645` target — and
the step after that doubles the DB to 47.24 GB. Budget the machine for the
step, not for today's count. (Under the old flat-0.75 target this same `(2,4)`
geometry would have run out of headroom in about ten days — ADR-0034 §4 —
which is why the target moved too, not just the arity.)

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

## 3.5 Remote deployment (AWS EC2 or any VPS)

The shape that makes the privacy story *architecturally* true — server and client on
different machines, different owners possible:

```
┌── EC2 / VPS ────────────────────────────┐        ┌── your laptop ──────────────────┐
│ risepir-rpc mainnet --partial|--snapshot│  LWE   │ risepir-rpc client              │
│   --bind 0.0.0.0                        │◄──────►│   --pir-url http://<server>:8645│
│   (PIR transport :8645 exposed;         │ vectors│   (hint + rewind + JSON-RPC     │
│    :8545 stays for local debugging)     │ deltas │    :8545 → cast / MetaMask)     │
└─────────────────────────────────────────┘        └─────────────────────────────────┘
```

The queried address **never leaves the laptop in any form** — the wire carries LWE
query vectors, responses, and the (public, identical-for-everyone) delta stream.
The client learns whether the server is complete or partial from `GET /mode` (its
`NotFound` policy is fetched, never guessed) and downloads the setup bundle once
(~100 MB per 1 M accounts; a few GB at the complete set — the inherent
SimplePIR-class client footprint; server restarts keep it valid via the state
file's bit-identical `A`/hints).

**EC2 steps** (assumes `aws-cli` configured; same recipe works on any VPS):

1. **Instance.** Partial demo: any 2 GB box (`t4g.small`). Complete set: 16 GB
   floor — `r7g.large` (2 vCPU/16 GB Graviton, ~$0.11/h on-demand, ~⅓ that on
   spot) or `r8g.xlarge` (32 GB) for comfort. Graviton/arm64 is fine — this
   project develops on Apple ARM; build with `target-cpu=native` on the box.
   30 GB gp3 EBS (snapshot shards + state file) ≈ $2.4/mo. **Spot is a
   reasonable fit**: the state file + catch-up replay make an interruption cost
   minutes, not a re-bootstrap.
2. **Security group.** Inbound: SSH (22) from your IP; **8645 only if** running
   the split client, ideally restricted to your IP. Do **not** expose 8545
   publicly — it is plain HTTP with no auth or rate limiting; for remote use of
   the co-located front end, tunnel instead: `ssh -L 8545:localhost:8545 <box>`
   (zero flags needed — the default loopback bind then just works).
3. **Build on the box** (cross-compiling from macOS to Linux is avoidable
   friction): install rustup + git, clone over HTTPS, and `cargo build
   --release -p risepir-rpc` — both this repo and the pinned IKPIR dep are
   public now, so the box needs no GitHub credentials. On a 2 GB instance add
   swap first (`fallocate -l 4G /swapfile …`) or build once on a larger spot
   box and copy the binary (same arch).
4. **Run** under `nohup`/`tmux`/systemd:
   `./risepir-rpc mainnet --partial --bind 0.0.0.0` (demo) or the full
   `--snapshot … --state …` form (§2.2). Then on the laptop:
   `./risepir-rpc client --pir-url http://<server-ip>:8645` and point
   `cast`/MetaMask at `http://127.0.0.1:8545`.
5. **Caveats to accept knowingly** (PoC): the PIR transport is plain HTTP —
   contents are protected by the crypto, but transport metadata (your IP, query
   *timing*) is visible on the wire; front it with a TLS proxy (caddy) if that
   matters. One `answer` round trip costs ~queries+responses of a few hundred
   KB at large scale (`docs/numbers.md` §4), so latency is network-dominated.

**Verified 2026-07-19** (two processes on this machine, same topology as above):
server `mainnet --partial --bind 0.0.0.0`, separate `client --pir-url` process —
setup bundle + `/mode` fetched over HTTP, then **5/5 private queries through the
split client byte-exact** vs publicnode, and strict not-found propagated through
it verbatim.

### 3.6 GCP walkthrough (written for someone who knows EC2, not GCP)

*(Unlike §1/§3.5's recorded runs, these commands could not be executed from the dev
environment — no GCP credentials exist there. They follow GCP's documented CLI
surface; expect to adjust names, nothing structural.)*

Concept map first — GCP is EC2 with different nouns:

| you know (AWS) | GCP equivalent | note |
|---|---|---|
| account | **project** | everything (VMs, buckets, BigQuery jobs) lives in a project; billing attaches to it |
| EC2 / instance | **Compute Engine** / VM instance | |
| instance type | **machine type** (`e2-standard-4` = 4 vCPU/16 GB) | |
| AMI | **image family** (`debian-12`) | |
| security group | **VPC firewall rule**, applied via instance **tags** | project-wide rules matched by tag |
| .pem key pairs | none — `gcloud compute ssh` generates/pushes keys itself | genuinely nicer |
| aws-cli | `gcloud` (+ `bq`, `gcloud storage` in the same SDK) | one install covers VM + BigQuery + GCS |
| spot | Spot VMs (`--provisioning-model=SPOT`) | same idea; our state file makes preemption cheap |

**One-time setup (on the MacBook):**

```bash
brew install --cask google-cloud-sdk
gcloud init                    # browser login; create project e.g. "<your-project-id>"; pick us-central1-a
```

Activate the **$300/90-day free trial** in the console (console.cloud.google.com —
it asks for a card for identity; when the trial ends resources are *stopped*, not
silently billed, unless you explicitly upgrade).

**Then link billing to the project — the step everyone trips on.** A billing
account is its own object in GCP: *every* project (in an organization or under
"No organization" — either is fine) must be explicitly linked to one, or
`services enable compute` fails with
`FAILED_PRECONDITION: Billing account … not found` (verified: exactly this
happened on first setup, 2026-07-19):

```bash
gcloud billing accounts list          # note the ACCOUNT_ID with OPEN: True
gcloud billing projects link <your-project-id> --billing-account=<ACCOUNT_ID>
```

(Older SDKs spell it `gcloud beta billing …`. Empty list ⇒ the trial was never
activated — do that first. A permission error on linking ⇒ the billing account
belongs to your organization and restricts outside projects — either link via
the console as the org admin, or Manage Resources → select the project →
Migrate it into the organization, then link.) Now:

```bash
gcloud config set project <your-project-id>
gcloud config set compute/zone us-central1-a     # us-central1: same multi-region as the BigQuery public data
gcloud services enable compute.googleapis.com
```

`us-central1` is deliberate: `bigquery-public-data` lives in the US multi-region,
so the GCS export bucket and the VM sit next to the data and the shard download to
the VM is free-to-pennies instead of internet egress.

**The VM** (partial demo: `e2-medium`, 4 GB, ~$24/mo-rate; complete set:
`e2-standard-4`, 16 GB, ~$98/mo-rate — both burn credit, not cash, for ~3 months):

```bash
gcloud compute instances create risepir \
  --zone=us-central1-a \
  --machine-type=e2-standard-4 \
  --image-family=debian-12 --image-project=debian-cloud \
  --boot-disk-size=50GB --boot-disk-type=pd-balanced \
  --tags=risepir
gcloud compute ssh risepir --zone=us-central1-a   # no key file to manage
```

(The explicit `--zone` makes these commands work even if the `config set
compute/zone` default above was skipped — without either, gcloud stops at an
interactive 130-zone menu, as first setup discovered. With the default set, the
flag is redundant but harmless. Operator note: from Southeast Asia,
`asia-southeast1-a` cuts interactive query latency ~5× at the cost of ~\$0.50 of
credit on the one-time cross-continent snapshot copy — either region works.)

**Firewall** — nothing needed for the SSH-tunnel shape (`gcloud compute ssh
risepir -- -L 8545:localhost:8545`, then cast against localhost). Only for the
split-client shape open the PIR port, restricted to your IP:

```bash
gcloud compute firewall-rules create risepir-pir \
  --allow=tcp:8645 --target-tags=risepir --source-ranges="$(curl -s ifconfig.me)/32"
```

(As on AWS: never open 8545 to the world.)

**On the VM** — identical to §3.5's build-on-box recipe: `sudo apt-get update &&
sudo apt-get install -y build-essential git curl pkg-config tmux`, install
rustup, clone over HTTPS (no GitHub credentials needed — both this repo and
the pinned IKPIR dep are public), `cargo build --release -p risepir-rpc`, run
in `tmux` with `--state`. The instance-create warning
about disk size vs 10 GB image size is expected and harmless — Debian grows
the root partition on first boot (`df -h /` shows the full disk). Pull the snapshot shards straight
from the export bucket: `gcloud storage cp 'gs://<your-bucket>/balances-*.csv.gz' .`
(if the VM's default service account lacks bucket read, the two-minute fix is
`gcloud auth login` on the VM and retry).

**Verified on GCP, 2026-07-19** (project `<your-project-id>`, `e2-medium`/Debian 12 in
`us-central1-a`, repo at 58cf7a5): clean-VM build in 4m09s; `mainnet --partial
--state` bootstrapped at finalized block 25,565,340 and followed live bursts;
**6/6 private queries byte-exact** vs publicnode *while the head advanced under
the check* (references at four successive heads, all exact); strict not-found
errored as designed; in-loop reconcile 8/8 exact. Setup quirks hit and folded
into this doc: the billing-link precondition, the interactive zone menu, the
one-repo deploy-key limit, and a passphrase-protected
`~/.ssh/google_compute_engine` blocking non-interactive `gcloud compute ssh`
(fix: regenerate the key passphrase-less, or `ssh-add` it).

**Graceful stop (learned the hard way, 2026-07-19):** launch the server with
`tmux new-session -d -s risepir "cd … && exec ./target/release/risepir-rpc …"`
— the `exec` makes the binary the pane process — and stop it with the anchored
`pkill -INT -f "^\./target/release/risepir-rpc"`, waiting for
`state saved; exiting` in the log before `instances stop`. A broad
`pkill -f risepir-rpc` also hits the tmux wrapper shell; tmux then SIGHUPs the
pane group and the server dies mid-save (0-byte `.tmp`; atomic tmp+rename means
the previous good state survives, and partial mode loses nothing — but a
complete-set deployment would lose its fast restart).

**Supervision (preferred over tmux):** [`ops/systemd/risepir.service`](../ops/systemd/risepir.service)
runs the same command under systemd — survives reboots, restarts on failure,
and encodes the graceful stop above as `KillSignal=SIGINT` +
`TimeoutStopSec=300`, so `systemctl stop risepir` *is* the
wait-for-`state saved; exiting` recipe with no wrapper shell to mis-signal.
Edit the `User=`/path lines to the VM login, `cp` to `/etc/systemd/system/`,
`systemctl enable --now risepir`; logs via `journalctl -u risepir -f`. The
tmux recipe stays valid for throwaway runs; anything that should outlive a
reboot belongs under the unit. A liveness probe is served at
`GET /healthz` on `:8645` (`ok <head-block>` — a stalling number is the
block-lag signal).

**State-file backup & restore (drill it once before trusting it):** the
state file is atomic-by-rename and, as of `RPST2`, carries a whole-file
xxh3 checksum, so a copy is a valid backup exactly when `load` accepts it.

```bash
# back up (server running is fine — copy the *state* file and its *.journal*
# sidecar together, never the .tmp; the autosave, ADR-0025, replaces the
# state file atomically by rename, so a copy taken at any moment is a
# complete file — either the previous save or the new one):
gcloud --quiet compute ssh risepir --command='cp ~/risepir-state.bin ~/risepir-state.bak && cp ~/risepir-state.journal ~/risepir-state.journal.bak'
# restore = stop server, put the backup in place, start; the server replays
# saved_block+1..finalized from the feed — the backup's age only costs
# replay time (~1–2 s/block), never correctness:
gcloud --quiet compute ssh risepir --command='cp ~/risepir-state.bak ~/risepir-state.bin && cp ~/risepir-state.journal.bak ~/risepir-state.journal'
```

The journal half of that copy is opportunistic, not required: a state file
alone always loads and follows correctly (ADR-0026 is additive). But a
`.journal` copied *separately in time* from its `.bin` is not necessarily
useless — its header's `base_digest` either matches the restored `.bin` (in
which case it is exactly as usable as it always was) or it does not, and a
mismatch is refused loudly at load, never trusted partially. Copying them
together is simply what guarantees a match every time, so backups restore
with whatever replay-avoidance the journal was providing rather than losing
it to a race between the two `cp`s.

A corrupt file (including a single flipped bit — the checksum catches what
structural checks cannot) is **rejected at load**; partial mode then
re-bootstraps empty, loss-free, while a complete-set deployment restores
the backup or re-runs the snapshot bootstrap. Legacy `RPST1` files load
with a warning and upgrade to `RPST2` on their next save.

**TLS (required the moment either port leaves localhost):** plaintext HTTP
means anyone on-path can *be* the operator (threat model §4.2), and the
browser front end additionally needs TLS for a non-localhost origin. Don't
teach the binary TLS — put a reverse proxy in front and keep the listeners
loopback-only. This is **deployed** as of 2026-07-26; the recipe below is
what runs, and §3.7 records it end to end.

```bash
sudo apt-get install -y caddy       # auto-provisions Let's Encrypt
sudo cp ops/caddy/Caddyfile /etc/caddy/Caddyfile   # edit the hostname first
sudo systemctl restart caddy
```

Serve **only** the PIR port (`:8645`) this way — it is what remote clients
and the browser front end need. `:8545` stays loopback/SSH-tunnel: it
answers *plaintext account queries* by design, so exposing it publicly
hands every visitor's queried address to the network (ADR-0012's warning,
one layer up).

**Cost hygiene:** this matters far more since the box became an `e2-highmem-8`
for the complete set — it burns **~$8.60/day** running, against ~$10/mo for the
250 GB disk when stopped. `gcloud compute instances stop risepir` when idle
(always `Ctrl-C` the server first and wait for `state saved; exiting`, so the
restart is a file load rather than a re-bootstrap — and since ADR-0025 the
server also rewrites the file every `--save-interval` anyway, so even a missed
Ctrl-C only costs the last ≤30 min of blocks as replay); `…delete` to zero it;
`gcloud billing projects describe <your-project-id>` / the console's Billing page shows
credit burn-down. After the credit: switch the same VM to Spot (~$95–130/mo at
64 GB) — Oracle's free tier is no longer an option at this size.

### Which cloud, if the goal is spending nothing

Re-costed 2026-07-26 against the working set the live deployment actually runs
at today (~38 GB, pre-ADR-0034 `(3,4)`, §2.3). **The complete set still has no
$0 option** — but the margin changed completely under ADR-0034's `(2,4)`
retune: that geometry's working set computes to **~24.7 GB** (§2.3), against a
24 GB Always-Free ceiling. The conclusion survives — 24.7 > 24, with nothing
left over for the OS, Caddy, or a save-time `.tmp` copy of the state file — but
it is now a **margin call, not the 1.6× overshoot** the old `(3,4)` working set
was. The rows below are what it actually costs, at whichever geometry is the
one being sized:

| option | complete-set cost | notes |
|---|---|---|
| GCP `e2-highmem-8` (8 vCPU/64 GB) + $300 credit | ≈ $0.36/h ≈ **$260/mo**, so ~5 weeks on the credit | **what this deployment runs on**; comfortable headroom at either geometry; you need GCP for the BigQuery export anyway, and same-region GCS→VM snapshot copy is free |
| GCP `e2-highmem-8`, stopped when idle | ~$10/mo disk only | the honest way to run a demo box: start it for a session, `Ctrl-C` to save state, stop it |
| AWS on-demand (`r7g.2xlarge`, 64 GB) | ≈ $0.43/h ≈ $310/mo | no free tier remotely near this RAM |
| AWS spot (`r7g.2xlarge`) | ≈ $95–130/mo | interruptions are cheap here (state file + catch-up replay) |
| Oracle Cloud Always Free (4 OCPU/**24 GB**) | $0 | **still not sufficient, but now only just** — the `(2,4)` working set is ~24.7 GB, ~0.7 GB over a 24 GB ceiling (was ~14 GB over, at the old `(3,4)` set's ~38 GB) — no headroom left for the OS or a save-time `.tmp` copy, so still a real no, just no longer a 1.6× one |
| this MacBook (16 GB) | $0 | partial mode only; the complete set is ~1.5× its total RAM at the deployed `(2,4)` geometry (was ~2.4× at `(3,4)`) |

The partial demo runs on anything ≥2 GB, including AWS free-tier-class instances.

## 3.7 Public HTTPS deployment (live, 2026-07-26)

The browser front end is reachable on the open internet at
**<https://demo.risepir.org>** — one hostname serving both the page and the
PIR transport, which is what ADR-0019's same-origin `connect-src 'self'` CSP
requires. Every command below was executed as written.

**Both the page and the transport must stay on one hostname.** Splitting them
(`app.` for the page, `pir.` for `/setup` and queries) would force the CSP
open and hand the client a second origin to trust; the same-origin shape is
load-bearing, not incidental.

The original origin, `private-eth-getbalance.duckdns.org`, is **still served
alongside** it (both names are in one Caddy site block) so links that predate
the move keep working. It is retained, not repointed elsewhere: a released
DuckDNS subdomain can be claimed by anyone, and it appears in earlier evidence
in this file.

Nothing else is exposed: the firewall opens **only 80/443**, both listeners stay
on `127.0.0.1`, and Caddy has no route to `:8545`.

**1. A registered domain and a static IP** (2026-08-17; `risepir.org`,
registered at Cloudflare, which is also the zone's DNS). The address is
reserved, so it survives stop/start:

```bash
gcloud compute addresses create risepir-ip --region=us-central1
gcloud compute instances delete-access-config risepir \
  --zone=us-central1-a --access-config-name="external-nat"
gcloud compute instances add-access-config risepir \
  --zone=us-central1-a --access-config-name="external-nat" \
  --address=136.115.93.177
```

Zone records:

| type | name | content | proxy |
|---|---|---|---|
| `A` | `demo` | `136.115.93.177` | **DNS only** |
| `CAA` | `@` | `0 issue "letsencrypt.org"` | — |
| `CAA` | `@` | `0 issuewild ";"` | — |

Three things about this are load-bearing:

- **`demo` is unproxied (grey cloud), deliberately.** Cloudflare's proxy would
  terminate TLS and put a third party in a position to serve a modified wasm
  client under this name — the code-delivery trust of ADR-0019 and threat
  model §4.2, widened by one party. Tempting, because `/setup` is 553.82 MB
  with no rate limiting behind it; the answer to *that* is roadmap C3/C5, and
  any CDN it brings must front the bundle without becoming the page's origin.
- **The CAA records** cut issuance for this name from "any public CA" to five,
  not to one, and the difference is invisible in the Cloudflare dashboard.
  Adding any CAA record to a Cloudflare-served zone makes Cloudflare inject
  further CAA records authorising its own CAs (Google Trust Services, SSL.com,
  Sectigo, DigiCert) so Universal SSL keeps working; they are not listed in the
  DNS UI and cannot be removed without disabling Universal SSL zone-wide. The
  `0 issuewild ";"` is inert for the same reason — CAA unions permissions
  rather than vetoing them. **Always read this policy with `dig CAA
  risepir.org`, never from the dashboard**; the two disagree (two records
  versus eleven). Threat model §4.2 carries the full accounting.
- **DNSSEC is on** at the zone, published to the `.org` registry by Cloudflare
  as both registrar and DNS.

The static address costs **~$3.60/mo**, billed at the in-use rate even while
the VM is stopped (GCP counts an address attached to a stopped instance as in
use; an unattached reservation bills higher). What it buys is the removal of
the previous shape's sharpest failure mode: under DuckDNS the external IP
changed on every `instances start`, so `~/duckdns-update.sh` had to run
*before* Caddy needed the name, and forgetting it left the origin pointing at
whoever now held the old address. **That ordering rule no longer exists** —
the IP never changes, and there is no update script in the start path.

**2. Firewall — 80/443 only, never the listeners.**

```bash
gcloud compute firewall-rules create risepir-web \
  --allow=tcp:80,tcp:443 --target-tags=risepir --source-ranges=0.0.0.0/0
```

**3. Caddy, staging first.** Production ACME issuance is rate-limited and a
misconfigured loop can lock you out for up to a week, so validate the whole
path — DNS, `:80` reachability, proxy — against the staging CA, which is not
rate-limited (browsers will warn; expected). Then remove the `acme_ca` line and
**restart** for the real certificate — `systemctl restart caddy`, not
`reload`. A reload does **not** re-issue across a CA change: storage is split
per issuer on disk, but the in-memory cache is keyed by hostname and is not
re-checked, so the reload silently issues nothing at all — no error, no
attempt, just an empty journal (§5.9 measured 10+ minutes of it, then 5
seconds after a restart). `ops/caddy/Caddyfile` is the deployed config.

**4. The server, as usual.** Caddy 502s until it is up:

```bash
tmux new-session -d -s risepir "cd ~/private-eth-getbalance && exec \
  ./target/release/risepir-rpc mainnet --partial --partial-capacity 1000000 \
  --web web --state ~/risepir-state.bin >> ~/server.log 2>&1"
```

### Verified on the public origin (2026-07-26, block 25616255–25616318)

The first time the page had ever run anywhere but `127.0.0.1`:

- `node web/test/browser.mjs https://private-eth-getbalance.duckdns.org` —
  **13/13**, including no CSP violations and no uncaught page errors, a real
  mainnet balance rendered (`0xfff064e0…c463` → 194008956570617841 wei), and an
  untracked address rendering as an error rather than `0`.
- `node web/test/e2e.mjs` against the served assets — **20/20**; the publicly
  served `client.wasm` hashes byte-identical to the VM's build
  (`4dee32bb…6617d`).
- `risepir-rpc client --pir-url https://private-eth-getbalance.duckdns.org` on
  a laptop — `0xfdd7992d…59fb` = 1791075509720685 wei, byte-exact against
  publicnode at the same explicit height; untracked → `-32000`.
- Certificate: Let's Encrypt, valid 2026-07-26 → 2026-10-24. `http://` → 308 →
  `https://`. `/setup` = 48,960,201 bytes in ~10 s (curl) / ~49 s (in-browser,
  cross-Pacific). Direct `:8645` and `:8545` from outside: unreachable.

One harness bug surfaced here and is fixed in the same change:
`web/test/browser.mjs` evaluated into the page while the initial navigation was
still in flight, so the execution context was destroyed under it and a *working*
page reported as a boot failure with no detail. It now waits for
`document.readyState === "complete"` and surfaces devtools-level errors instead
of returning `undefined`. On loopback the race never fired.

### What this deployment costs, and what it does not survive

Nearly free beyond the VM hours: **~$11.20/yr** for `risepir.org` (Cloudflare
Registrar sells at registry wholesale, with no renewal premium) and
**~$3.60/mo** for the static IP — about **$54/yr fixed**, against $8.60 per
*day* whenever the VM runs. Let's Encrypt, Caddy and Cloudflare DNS are free,
and a handful of visitors is well under a gigabyte of egress. The cost control
that matters is unchanged: stop the instance when idle.

It is sized for **a link sent to a few colleagues**, not for public traffic.
Cold visitors now share a single cached `/setup` encode and get a refcounted
clone of it rather than one encode and one buffer each (ADR-0028), so
concurrent bootstraps cost bandwidth rather than multiplying memory — the
per-route `SETUP_MAX_CONCURRENT = 2` cap that used to sit here is gone, having
been measured not to bound what it claimed (tower released its permit when the
handler returned, not when the transfer finished). What remains undefended is
unchanged and is the part that matters: there is **no rate limiting at all**,
and the egress of a large `/setup` bundle per cold visitor is still entirely
real (threat model §3 names volumetric DoS as undefended) — **553.82 MB** per
cold visitor today (`content-length: 553819345`, measured on the wire
2026-08-17, §5.9). This paragraph previously said 830.73 MB on the grounds
that the origin had not been re-bootstrapped since ADR-0034; it has been
twice since (§5.4 and §5.8), so `(arity 2, bucket_size 4)` is what it
actually serves. `/setup` behind a CDN plus per-IP quotas is roadmap
C5/C3 — do that before sharing the link wider.

**Certificate renewal needs the VM up occasionally.** Caddy renews from ~30 days
before expiry over `:80`. Since this VM is stopped between demos, a gap longer
than that window lets the certificate lapse and visitors get a hard TLS error;
starting the VM for any demo inside the window fixes it automatically.

This is why **`demo.` is the wrong hostname to cite in a paper.** A reader who
clicks a printed URL during one of the many days the VM is stopped gets a
connection or TLS failure, and a dead demo link reads as abandoned work. The
intended shape is an always-on static page at the apex `risepir.org` — served
by something that is not this VM, describing the system with screenshots and
numbers, and linking onward to `demo.risepir.org` with a plain statement that
the live instance runs during demos and on request.

**That page is now live** (2026-08-17, ADR-0043). `risepir.org` and
`www.risepir.org` are served by **Cloudflare Pages**, which is free, always on,
and adds no party that could not already redirect this name — Cloudflare is
already the zone's registrar and DNS. It carries no cryptographic client and
makes no PIR queries; it is prose, the measured numbers, screenshots of a real
lookup taken while the demo was up, ADR-0019's residual-trust disclosure, and a
link onward. Verified serving with a valid certificate **while the VM was in
`TERMINATED` state** — which is the whole point of it:

```
$ gcloud compute instances describe risepir --zone=us-central1-a --format='value(status)'
TERMINATED
$ curl -sS -o /dev/null -w '%{http_code}\n' https://risepir.org/
200
$ echo | openssl s_client -connect risepir.org:443 -servername risepir.org 2>/dev/null \
    | openssl x509 -noout -issuer -subject -dates
issuer=C=US, O=Google Trust Services, CN=WE1
subject=CN=risepir.org
notBefore=Aug 17 09:28:01 2026 GMT / notAfter=Nov 15 10:27:59 2026 GMT
$ curl -sS -o /dev/null --max-time 12 https://demo.risepir.org/mode
curl: (28) Connection timed out after 12004 milliseconds
```

Note the apex certificate comes from **Google Trust Services** (Cloudflare
Universal SSL), not Let's Encrypt — permitted by the `pki.goog` CAA record
Cloudflare injects into any zone it serves that has CAA at all, which §3.7's
CAA note above already explains. The `demo.` record is untouched: still a
DNS-only `A` at `136.115.93.177`, never proxied.

**So cite `risepir.org`, not `demo.risepir.org`.** The apex resolves whether or
not anyone is paying for the VM that week; `demo.` remains the
intermittently-available origin, and the apex says so in plain words rather
than letting a reader discover it by clicking.

**The trust chain is narrower than it was, and still not empty.** Whoever
controls DNS for this name can obtain a valid certificate for it — DNS control
is all Let's Encrypt checks — and serve a modified wasm client under it. That
is inherent to web delivery, not a property of the registrar, and it is the
same category as ADR-0019's disclosed code-delivery trust. What the registered
domain changed is *who* that is: a Cloudflare account with 2FA and a registrar
lock, rather than a free-subdomain provider and a bearer token with neither,
plus DNSSEC and CAA that a free subdomain could not carry. See threat model
§4.2 and §8.

## 4. Operational notes (the never-wrong-answer contract, operationally)

- **`CRITICAL` in the log** (apply failure, reconcile mismatch, corrupt store):
  following has stopped; serving continues at the last applied block. Fix =
  re-bootstrap from a fresh snapshot (delete/replace the state file). Never
  restart-and-hope into a drifted state.
- **`GET /healthz` now also reports the reconcile check's own health**
  (ADR-0027) — watch it, because the check can go completely dark (the
  independent provider refusing every fetch) without a single mismatch ever
  firing, and that looks like silence, not an alarm, unless something is
  reading these fields. Example body:
  ```text
  ok 25617400
  reconcile_configured=1
  reconcile_last_checkpoint_block=25617400
  reconcile_last_checkpoint_unix=1785079800
  reconcile_last_success_block=25617400
  reconcile_last_success_unix=1785079800
  reconcile_comparisons_total=1848
  reconcile_checkpoints_total=231
  reconcile_consecutive_dark=0
  reconcile_halted=0
  ```
  The first line is always exactly `ok <head>`, unchanged; everything below
  it is additive `key=value` lines. The signal to watch is a climbing
  `reconcile_consecutive_dark` alongside a `reconcile_last_success_unix` that
  stops advancing — the log itself already escalates to `CRITICAL` at 20
  consecutive dark checkpoints (~2h, `mainnet.rs`'s
  `DARK_ESCALATION_THRESHOLD`), so this need not be polled continuously, but
  a dashboard on these fields catches it sooner.
- **`GET /metrics` and `GET /status` (ADR-0039)** answer "is the follow loop
  behind, and by how much" without SSH-ing in and tailing a log — the gap
  that took 35 minutes to close by hand on 2026-07-28. `/metrics` is
  Prometheus text exposition (`text/plain; version=0.0.4`, hand-rolled, no
  new dependency): `risepir_head_block`/`risepir_finalized_block`/
  `risepir_block_lag` (the single most useful number — the follow loop's
  own `finalized` poll, previously discarded every iteration, now
  published), an answer-latency histogram, per-route request/error
  counters (`class` is always a fixed `ServerError`/`WireError` variant
  name, never a formatted message), store occupancy, `/setup` cache
  regenerations, state-save/journal outcomes, and every `reconcile_*`
  field above, as proper gauges/counters instead of `key=value` text.
  `curl -s <host>:8645/metrics` works from any box with network access to
  the PIR port. `/status` (served only with `--web`, same mechanism as the
  browser front end) is the same data as a small page that polls
  `/metrics` every 5 s — open it in a browser instead of reaching for
  `curl`. Both are public wherever Caddy proxies the whole PIR port, as it
  already does (§3.7) — every field is an aggregate, never per-query, but
  see `docs/threat-model.md` §5/§8 for what publishing them means for this
  deployment's traffic-analysis posture, and `ops/caddy/Caddyfile`'s
  commented-out stanza for taking public exposure back without touching
  the binary.
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
- **One process per `--state` path, enforced**: the server holds an advisory
  lock on `<state>.lock` for its lifetime; a second start on the same path
  (the classic double-`tmux` accident) fails fast with "another process
  already holds …" instead of both writers interleaving into the same
  `<state>.tmp` and destroying the good multi-GB file (36 GB at the live
  `(3,4)` deployment today, ≈24 GB once re-bootstrapped to the deployed
  `(2,4)` geometry — ADR-0034). The `.lock` file itself
  is empty and harmless — advisory locks die with the process, so it never
  blocks a restart and needs no cleanup or backup.
- **Lineage epochs (ADR-0033)**: `/sync`, `/answer`, and `/delta/{block}` now
  require the `?epoch=` token clients read off `/setup` (`x-risepir-epoch`),
  and refuse (`409`/`404`) across a mismatch — so a client that survived a
  server re-bootstrap can never be fed the new lineage's deltas against its
  old hint (which could decode to a silent wrong `0x0` in complete mode).
  Operationally: the **first redeploy that crosses this change logs one wave
  of 409s** from pre-existing clients — open browser tabs show their normal
  "reload the page" guidance, older CLI builds re-bootstrap once and then
  error honestly; both heal by re-fetching `/setup` with current code. A
  restart from the *state file* keeps its epoch (the LWE seeds are
  persisted), so routine restarts do not invalidate anyone; only a genuine
  re-bootstrap (fresh snapshot ingest) mints a new lineage — which is
  exactly when old clients *must* be refused.

### Log timestamps and rotation

**Every runtime log line carries an RFC 3339 UTC timestamp** (since
2026-07-30). The message text after it is byte-identical to what the same
line printed before, prefixes included, so existing greps still match:

```text
2026-07-30T04:12:33Z risepir-rpc mainnet: state saved (autosave): block 25637839, 24.18 GB in 175.6s (138 MB/s)
```

The one thing this breaks is a **`^`-anchored** pattern against a log line —
`grep "^risepir-rpc mainnet: ..."` must drop its anchor. Timestamps come from
`crates/risepir-rpc/src/logging.rs` (`logln!`), which is dependency-free for
the same reason ADR-0039's metrics exposition is hand-rolled.

Note the deliberate split: **log records are timestamped, CLI output is not.**
The startup banner, the usage text and argument errors all still print plain
on stdout — they are interactive feedback, not a record anyone greps later.

Why this mattered enough to add: reading this deployment's log on 2026-07-29
meant answering "when did the dark-reconcile window end" and "how long did
that `--hard-refresh` take" by correlating block numbers against file mtimes.
Block height is a fine clock *inside* the chain and useless for joining
against anything outside it — a provider's status page, a `dmesg`, an
autosave.

**Rotation is not automatic in the tmux shape.** The log is a plain file the
shell opened with `>> ~/server-complete.log 2>&1`; nothing truncates it. It
reached **66.79 MB** before the first rotation setup, of which 67,791 lines
were a single pre-ADR-0041 failure mode. Install the provided config:

```bash
sudo cp ops/logrotate/risepir /etc/logrotate.d/risepir
sudo logrotate --debug /etc/logrotate.d/risepir   # dry run
sudo logrotate --force /etc/logrotate.d/risepir   # rotate once, now
```

**`copytruncate` in that config is load-bearing, not a style choice.** The
server never opens its own log — the shell opens it before `exec`ing the
binary, so the process holds one descriptor for its whole run and never
reopens. Under logrotate's default rename-then-create the process would keep
writing to the *renamed* inode forever: `server-complete.log` would sit at
0 bytes while `server-complete.log.1` grew, which reads exactly like "the
server stopped logging". And do **not** reach for the usual
`create` + `postrotate kill -HUP` remedy: this binary installs no SIGHUP
handler, so the default disposition applies and the signal would *terminate*
the server — an ungraceful stop, which at the complete set means a journal
replay on the way back up.

If you run the systemd unit (`ops/systemd/risepir.service`) instead, none of
this applies: journald rotates on its own.

### Migration: the `xxh3_128` pin bump — REQUIRED, and NOT YET RUN

**Status: planned, not executed.** The VM has been `TERMINATED` since
2026-07-29 (block 25,638,894) and was deliberately left alone while this
landed. Nothing below has been run against it; every figure is a projection
from measured local numbers and the §5.4 round, and is labelled as such.

**Why a restart will not do.** The pin moved to `0f3b99b`, which changes the
primitive's item hash from `xxh3_64` to `xxh3_128`. Every key now lands in a
different bucket. The **geometry is unchanged** — ADR-0042 kept
`fingerprint_bits = 32`, and `plaintext_bits` is still 8, `cells_per_slot`
still 22, server DB still 23.62 GB — so the usual guards are blind to this by
construction: they compare parameters, and the parameters are correct. The
state file's format version is what carries the hash lineage now, so the
existing `~/risepir-state.bin` (an **`RPST2`** file) is refused by name:

```
state file is RPST2, written before the primitive's item hash changed from
xxh3_64 to xxh3_128 — every key hashes to a different bucket now, so these
cells no longer describe a filter this binary can read, and loading them
would miss on every lookup (answering 0x0 for accounts that exist). This is
not disk corruption, and the geometry is unchanged, which is exactly why
nothing cheaper catches it; move the --state file aside and re-bootstrap
from a fresh snapshot (do not restore from backup, the file itself is fine)
```

That refusal is the whole point: without it the server would have come up
clean and answered `0x0` for every account.

**The sequence.** No DNS step: since 2026-08-17 the external IP is the reserved
`136.115.93.177` and does not move across stop/start (§3.7).

```bash
gcloud compute instances start risepir
gcloud --quiet compute ssh risepir --command='cd ~/private-eth-getbalance && \
  git pull && cargo build --release -p risepir-rpc && \
  cargo run -p xtask --release -- web'
# The state file must move aside, or --snapshot is silently ignored:
gcloud --quiet compute ssh risepir \
  --command='mv ~/risepir-state.bin ~/risepir-state.bin.rpst2-20260731'
gcloud --quiet compute ssh risepir --command='~/bootstrap-complete.sh'
```

**Projected cost** (PROJECTION — the only measured anchor is §5.4's own
`(2,4)` bootstrap):

| step | expectation | basis |
|---|---|---|
| bootstrap | **~16 min** (8 min ingest + ~6 min PIR setup + 2 min save) | §5.4, measured, same geometry |
| server DB | **23.62 GB**, unchanged | `numbers.md` §4b, geometry did not move |
| state file | **~24.18 GB**, unchanged | §5.4; only placements move, not sizes |
| snapshot → head replay | **~1 s/block** — the expensive part | §5.4 |
| replay depth | stopped at 25,638,894; **budget hours**, and growing daily | — |
| meter | **~$8.60/day** while running | §2.3 |

The replay dominates and grows with every day the box stays down; the
snapshot's own age adds to it (deploy.md §2.1). Nothing about this is urgent
— the deployment is *stopped*, not serving wrong answers — so the honest
framing is that the cost rises slowly until someone chooses to spend it.

**The rollback story, honestly.** There is none that is a restart. The
superseded `RPST2` file cannot be loaded by any binary built from this tree —
that is exactly what the version check guarantees — so "roll back" means
reverting the pin to `3d60fa7` *and* re-bootstrapping anyway, since the
`RPST2` file will by then be stale by however long the box ran. Keep the moved
file only as evidence, not as a recovery path, and delete it once the new one
is verified (the `(3,4)` file's 33.77 GB were reclaimed on the same reasoning,
§5.4). Plan forward, not backward.

**Verify after, before declaring it done**: `GET /mode` returns `1`
(complete); a handful of `eth_getBalance` answers byte-match an independent
provider at an explicit height (never `"latest"`-vs-`"latest"`, ADR-0007); the
in-loop reconciler reports exact at its checkpoints; and only 80/443 are
reachable from outside.

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

### 5.1 Browser front end, live (2026-07-21)

`mainnet --partial --partial-capacity 1000000 --web web` on a laptop, keyless dRPC
feed, real LWE parameters — the whole lookup performed **inside a browser page**:

- Bootstrapped empty at finalized block 25,580,831; the page loaded the 49 MB hint
  and answered from it while the server kept advancing (the rewind, not a
  re-download: hint pinned at 25,580,894, client caught up to 25,580,895, 7,606
  delta cells pending).
- `0xffda2922535c1d5c12b6fec01b15186be39787af` → **44033478848147061 wei** at
  finalized block 25,580,895, displayed in the page. Independently confirmed
  **byte-exact** by `rpc.flashbots.net` at that same height.
- `0xfffc65b9c0fc94a34f1e5d9f2a78623f37f47fc8` → **3148629001800 wei** at block
  25,580,844, likewise byte-exact against `rpc.flashbots.net` (`1rpc.io` had
  already pruned that state — unavailable, not disagreeing).
- What crossed the wire for that lookup: **39,335 bytes** of LWE query, 38,419
  bytes of response, 22,525 bytes of public delta. Zero addresses, in any form.
- An untracked address rendered as an **error** explaining why it will not answer
  `0`, never a zero balance.
- Gates, both green against this deployment: `node web/test/e2e.mjs` (18 checks,
  including "the wasm module's only import is the entropy shim" and "two queries
  for one address ship different ciphertext of identical length") and
  `node web/test/browser.mjs` in headless Brave (12 checks, including no CSP
  violations and no uncaught page errors).
- Full lookup round trip: **41 ms** over loopback, of which ~10 ms is client
  crypto.

### 5.2 Public HTTPS origin, live (2026-07-26)

The same front end on the open internet at
`https://private-eth-getbalance.duckdns.org`, TLS terminated by Caddy in front
of a loopback-only `:8645`. Evidence, commands and caveats are recorded inline
with the runbook in **§3.7** rather than duplicated here — including the two
gates re-run against the public origin (13/13 and 20/20), the CLI client driven
over the public URL, and the harness race that only a non-loopback origin
exposed.

### 5.3 The COMPLETE mainnet set, live (2026-07-26)

> **Note (ADR-0034, added after the fact): this section is pre-ADR-0034
> evidence, left exactly as recorded.** Every figure below — 35.43 GB,
> `100663296` buckets, load 0.498, 1236.5 s setup, a 36.26 GB state file,
> `arity=3 setup=830.73 MB` on the wire, ~1.66 GB in the browser — describes
> the `(arity 3, bucket_size 4)` bootstrap **as it actually happened**. It is
> **no longer what the live server serves**: the box was re-bootstrapped onto
> `(arity 2, bucket_size 4)` on 2026-07-27, and the measured numbers for that
> are in **§5.4**. Read this section as the `(3,4)` baseline the ADR-0034
> figures are compared against, not as the current deployment.

**Stage 1.d, done.** The deployment at `https://private-eth-getbalance.duckdns.org`
serves the complete nonzero-balance set — every one of mainnet's 200,503,969
funded accounts — not the partial demo. `GET /mode` returns `1`.

Machine: GCP `e2-highmem-8` (8 vCPU / 62 GB usable, 250 GB pd-balanced),
upgraded in place from the `e2-medium` that ran the partial demo.

**Bootstrap, end to end (~33 min of CPU):**

| step | measured |
|---|---|
| gate query | 200,503,969 nonzero accounts; snapshot block 25,613,233; dataset head `2026-07-25 23:59:59` |
| export | 321 shards, 5.64 GiB gzipped; GCS→VM in **13 s at 669 MiB/s** |
| geometry (printed before allocating) | `100663296 buckets, server DB 35.43 GB, load 0.498` |
| snapshot ingest | **734 s** — 200,503,969 rows, 200,503,969 nonzero, **0 zero skipped**, max balance 88,453,361,538,334,634,086,007,430 wei |
| PIR setup (one-time) | **1236.5 s** at block 25,613,233 — all 8 cores busy (load avg 7.99), RSS 32.5 GB |
| state file | **36,264,209,243 B (36.26 GB)** |
| peak RSS | **~37 GB** of 62 GB |
| restart from state | **135.8 s** — "block 25613849, 200510802 accounts, complete set" |

**Caught up to the head.** The snapshot→head replay closed the full
~3,500-block gap and the server reached **finalized exactly** (`GAP=0` at block
25,617,400). Once inside publicnode's recent window its own in-loop backstop
came back: `reconcile at block 25617390: 8 account(s) exact vs independent
provider`, with **zero `CRITICAL` and zero mismatch lines** across the entire
run.

**Correctness, against an independent provider.** `rpc.flashbots.net` — neither
the feed (dRPC/merkle) nor the in-loop reconciler (publicnode), and the only
surveyed keyless endpoint serving *archive* `eth_getBalance`, which is what a
still-catching-up server has to be checked against:

- **11/11 private `eth_getBalance` byte-exact**, all at the single explicit
  height 25,617,400, through a `risepir-rpc client` doing the real PIR path
  (query → answer → rewind → joint-mask scan). Re-run mid-catch-up against
  bracketed heights it was likewise 11/11.
- Two of those were accounts **absent** from the set, answering exactly `0x0`
  and matching the provider's `0x0` — and a never-funded address likewise
  `0x0`. This is the complete-set semantic the partial deployment is forbidden
  to offer (ADR-0015/0017), demonstrated live.

**Both browser gates green against the public origin** (Node ≥ 22 required, §1.5):

- `e2e.mjs`: **0 failing checks**, reporting `mode=COMPLETE pinned=25613989
  arity=3 setup=830.73 MB` — the computed §4c figure, confirmed on the wire.
- `browser.mjs`: **11/11**, including "an absent address in a complete set
  shows 0 and explains that absence means zero", no CSP violations and no
  uncaught page errors. The page boots in **153 s** over the public internet at
  this scale (vs seconds at 49 MB), and holds ~1.66 GB — it works, and it is
  emphatically the point at which the CLI client is the better tool.

**Two things that went wrong, both now fixed in code:**

1. **The follow loop wedged permanently at block 25,613,828** — dRPC's free
   plan refuses that block deterministically (`HTTP 408`), and since a block
   may never be skipped the loop retried it 55 times and stopped advancing.
   Fixed by ordered feed fallbacks (ADR-0024, PRs #7/#8). The first cut of that
   fix then aborted startup on a transient fallback `403`, on a server holding
   the 36 GB state file — hence the by-position strictness now documented in
   the ADR.
2. **The co-located `:8545` front end stalled during catch-up** with
   `server is resyncing (the client fell behind the server's retained delta
   window)`. Fast replay outruns the delta ring, and the bootstrapped-at-startup
   client never recovers. It fails loudly rather than answering wrongly — the
   contract holds — but **a long catch-up leaves the co-located front end
   unusable until restarted**. A freshly started client (CLI or a browser
   reload) is unaffected, which is how the verification above was run.

**Also worth knowing:** in-loop reconciliation is *silently unavailable*
throughout a catch-up from an old snapshot — publicnode's keyless tier refuses
archive depths, so every checkpoint logs a fetch failure rather than a
comparison (685 of them in the first hour). No mismatch was ever reported, but
"no mismatch" means "not checked" here, not "checked and agreed". The
independent verification above exists precisely because that backstop was down.

### 5.4 Re-bootstrapped onto `(arity 2, bucket_size 4)`, live (2026-07-27)

The operation §5.3's note said had not happened yet, done on the live box, from
the same 321 shards §5.3 used. Sequence: graceful `SIGINT` (state saved, 36 GB),
`instances stop` → `TERMINATED`, `instances start`, `duckdns-update.sh` **first**
(the external IP moved to a new ephemeral address), then `git` `49052b3 → c274737`
(16 commits), rebuild, move the old state file aside, `~/bootstrap-complete.sh`.

**The refusal fires first, and by name.** Pointing the new binary at the existing
36.26 GB state file, before touching anything else:

```
risepir-rpc mainnet: loading state from /home/admin/risepir-state.bin.arity3-20260727 ...
risepir-rpc mainnet: fatal: loading …: state file rejected: state file geometry
is arity 3 but this binary is compiled for arity 2 (ADR-0034) — this is not disk
corruption, it is an intact state file from a previous geometry lineage; move the
--state file aside and re-bootstrap from a fresh snapshot (do not restore from
backup, the file itself is fine)
```

`exit 1`, immediately after the header decodes and before any of the multi-GB
cells section is read (ADR-0034 §6). A restart across a geometry change fails
loudly rather than serving the wrong lineage — verified, not assumed.

**Bootstrap, end to end:**

| step | `(3,4)` 2026-07-26 | `(2,4)` 2026-07-27 |
|---|---|---|
| geometry (printed before allocating) | `100663296` buckets, 35.43 GB, load 0.498 | **`67108864` buckets, 23.62 GB, load 0.747** |
| snapshot ingest | 734 s | **483 s** |
| PIR setup | 1236.5 s | **≈361 s** (derived) |
| final save | — | **115.2 s** |
| state file | 36,264,209,243 B | **24,176,139,523 B** |
| whole bootstrap | ~33 min | **959 s (16 min)** |
| peak RSS | ~37 GB | **~24.6 GB** of 62 GB |

Ingest rows matched §5.3 to the byte — 200,503,969 rows, 200,503,969 nonzero,
**0 zero skipped**, max balance 88,453,361,538,334,634,086,007,430 wei — which is
the cheapest available check that the two lineages ingested the same set. Setup
time is *derived* (bootstrap start → state-file mtime, minus logged ingest and
save), not logged directly; the others are logged.

**On the wire.** `GET /setup` = **553,819,345 B**: the computed 553,819,200 B
hint (§4c) plus 145 B of framing, served in **0.208 s** on loopback — ADR-0028's
single shared encoded response, not a per-client encode.

**Both browser gates green against the public origin** (run from a Mac on Node 24;
the VM's Node 18 is too old for them):

- `e2e.mjs`: **0 failing checks**, reporting `mode=COMPLETE pinned=25613520
  arity=2 setup=553.82 MB` — the computed §4c figure confirmed on the wire, at
  the new arity. Also covers ADR-0033's lineage epoch being exposed, and
  ADR-0032's capacity preflight admitting a `deviceMemory=4` phone at the
  ADR-0034 hint size that the pre-ADR-0034 size refused.
- `browser.mjs`: **11/11** in real headless Brave, no CSP violations, no uncaught
  page errors.

TLS verifies on the public origin after the stop/start, and `:8545`/`:8645` both
still refuse connections from off-box (re-verified 2026-07-27).

**Three things that were broken in §5.3 and are observably fixed here**, all in
the same catch-up rather than in a test:

1. **The co-located `:8545` front end no longer wedges.** §5.3's second failure
   was fast replay outrunning the delta ring, leaving it unusable until
   restarted. It happened again — and ADR-0029 handled it:
   `risepir-rpc: re-bootstrapped after falling out of the server's retained
   delta window (pinned block 25614119 -> 25614998, mode complete)`. It answered
   continuously across that, which is *how* the byte-exact checks above were
   run through it.
2. **The blind reconciler now says so.** publicnode still refuses archive
   depths, so a catch-up from an old snapshot still cannot reconcile — but
   ADR-0027 turns that from silence into
   `reconcile: WARNING: … 236 failed (dark checkpoint #7 in a row); no
   successful comparison yet this run`. "Not checked" no longer reads like
   "checked and agreed".
3. **State no longer depends on a clean shutdown.** ADR-0025's autosave ran on
   schedule during the replay (`state saved (autosave): block 25616711,
   24.18 GB in 123.4s (196 MB/s)`), and ADR-0026's journal sidecar is being
   written at `~/risepir-state.journal` with `--journal-restore` OFF — the
   soak configuration, reporting rather than restoring, exactly as intended
   before that switch is trusted.

#### The snapshot is not exact at its own boundary — and the replay is what fixes it

Verifying byte-exactness against independent archive providers mid-catch-up
turned up mismatches, and running them down produced a result worth recording,
because it bears directly on the never-wrong-answer contract and it is a property
of **the exported ground truth**, not of the PIR path. It applied identically to
the `(3,4)` deployment; the geometry change did not cause it.

Method: compare the exported CSV rows *themselves* — not the server — against
`gateway.tenderly.co/public/mainnet` and `eth-mainnet.public.blastapi.io`
(neither is the feed, dRPC/merkle, nor the in-loop reconciler, publicnode) **at
the snapshot block 25,613,233**, whose timestamp is `2026-07-25T23:59:59Z`,
exactly the export's dataset head. `rpc.flashbots.net`, used for this in §5.3,
returned `504` all day and was unusable.

- **Accounts drawn at random: 40/40 byte-exact.** For the overwhelmingly dormant
  bulk of 200 M accounts, the export is right.
- **Accounts active in the blocks immediately before the boundary: 6 of the 27
  present were wrong**, by `−424.64`, `+33.72`, `−4.14`, `−2.78`, `−0.88` and
  `−0.04` ETH. Sign varies, so this is not a uniformly-missing-credit story.
- **One funded account was missing from the export outright** — from a separate
  25-address draw of the same population, then confirmed by a clean single pass
  over all 321 shards: `0x3b4d794a66304f130a4db8f2551b0070dfcf5ca7`, holding
  **2,790.43 ETH** at the snapshot block, had no row at all. In a *complete* set
  an absent account is not "unknown", it is a definitive `0x0` (ADR-0015/0017) —
  so that is the worst-shaped wrong answer this system can give, and it came
  from the data, not the code.

**What saves it is that the replay writes absolute post-state, not deltas.** The
prestate tracer gives the true balance for every account a block touches, so a
wrong row is corrected the first time that account is touched — and a *missing*
row is inserted. Both halves were observed directly during the catch-up:

| account | served from snapshot | after replay touched it | independent provider |
|---|---|---|---|
| `0xd8dA…6045` | `0x5c03cea37fe9d896` (wrong) | `0x5c0aabfdffd2d737` | `0x5c0aabfdffd2d737` ✓ |
| `0x3b4d…5ca7` | absent → `0x0` (wrong) | `0x94ca9c9f9fdd198c00` (2,744.72 ETH, at block 25,616,237) | `0x94ca9c9f9fdd198c00` ✓ |

Because the wrong rows are, by construction, the *active* ones, most heal within
the first hours of replay. The residual exposure is precise and worth stating:
**an account whose export row is wrong and which is then never touched again
stays wrong**, and nothing in the current design will notice — the in-loop
reconciler samples too few accounts to find it, and (see §5.3) it is blind
altogether at archive depths during a catch-up. ADR-0027's health line now at
least makes that blindness visible while it lasts:

```
reconcile: WARNING: block 25613430: 236 fetch(es) attempted against the
independent provider, 236 failed (dark checkpoint #7 in a row); no successful
comparison yet this run
```

The honest summary is that a complete-set deployment is only as correct as its
snapshot, that this snapshot is measurably not exact at its own boundary, and
that the system's self-healing turns that from a permanent error into a
transient one for every account that keeps transacting. `docs/HANDOFF.md` carries
the remedy.

#### Caught up, and the backstop came back

The snapshot→head replay closed the whole **13,004-block** gap — 25,613,233 →
**25,626,237** — in **4 h 20 min** at a steady **0.83 blocks/s**, serving the
whole time, labelled with the block it was as of. The same ten-account check
was re-run as each of the three wrong accounts was touched, and the score moved
exactly as the healing model predicts:

| server head | byte-exact | what had just healed |
|---:|---|---|
| 25,614,119 | 7 / 10 | — (the three snapshot errors, all still wrong) |
| 25,616,790 | 8 / 10 | `0xd8dA…6045` |
| 25,619,061 | 9 / 10 | `0x0000…dEaD` |
| 25,623,043 | 10 / 10 | `0x742d…f44e` |
| **25,626,237 (head)** | **10 / 10** | — |

The final run is at the single explicit height **25,626,237**, against both
independent providers: six funded accounts byte-exact, and four absent ones
answering exactly `0x0` — the complete-set semantic a partial deployment is
forbidden to offer, demonstrated live again on the new geometry.

**The in-loop backstop returned the moment it could**:
`reconcile at block 25626210: 8 account(s) exact vs independent provider`. It
had been dark for **432 consecutive checkpoints**, and ADR-0027 escalated that
from `WARNING` to **21 `CRITICAL` lines** on the way — which is the point of
that ADR: §5.3's run produced 685 silent fetch failures and read as clean.
Across the entire run: **zero balance mismatches, zero halts.** Every one of
those 21 `CRITICAL`s is about the *checker* being unavailable, never about an
answer being wrong.

`e2e.mjs` was re-run against the public origin once caught up and passes there
too (**0 failing checks** at block 25,626,269), so the gates cover both ends of
the replay, not just the bootstrap.

Housekeeping: the superseded 36.26 GB `(3,4)` state file was kept on the box as
`~/risepir-state.bin.arity3-20260727` (disk was at 70 G of 246 G) as the
rollback if `(2,4)` ever needed reverting.

**Deleted 2026-07-29** — disk went **92 G → 58 G of 246 G** (39% → 25%), with
the live 24,176,139,523 B state file verified byte-for-byte unchanged either
side of the `rm`. The rollback it offered was always weaker than it sounded:
`STORE_ARITY` (ADR-0034 §6) means *no* binary built from this tree can load a
3-ary file, so reverting was never "point `--state` at it and restart" — it was
a code revert to the 3-ary lineage *and* a replay onto a file that grows staler
every day (it was pinned at block 25,613,233 + change, days behind head by the
time it was removed). `~/bootstrap-complete.sh` regenerates the `(2,4)` set in
~16 min; regenerating a `(3,4)` set would need the old geometry constants back
first. If `(2,4)` ever does need reverting, a fresh bootstrap is the honest
path, and this file would not have shortened it.

The same-lineage backup `~/risepir-state.bin.pre0728` (22.52 GB) is **kept** —
that one the current binary *can* load.

### 5.5 An autosave is invisible to serving, measured (2026-07-28)

ADR-0025 argues that a full state save cannot stall `/answer`, because the save
runs in the follow loop's own task under a read guard and the follow loop is
`NodeState`'s only writer, so no writer can queue behind it. Until now that was
an argument plus a unit test (`queued_writer_parks_new_readers`). It is now also
a production measurement, taken on the live box at the complete set while it
wrote the real 24.18 GB file.

Two probes against `https://private-eth-getbalance.duckdns.org`, one every 5 s
for 35 min: `GET /head`, and `POST /answer?epoch=<current>` with a deliberately
malformed 4-byte body — that second one matters because the handler takes the
state read lock *before* it decodes, so its round-trip measures exactly what a
real query would wait for, without needing a 553.82 MB hint to produce one.

| probe | samples | failures | max |
|---|---|---|---|
| `GET /head` | 414 | 0 non-200 | 1.71 s |
| `POST /answer` (garbage body → `400`) | 344 | 0 non-400 | 1.35 s |

The autosave in that window ran `04:10:46 → 04:12:55` UTC — 128.6 s, 24.18 GB at
188 MB/s. Twenty-five samples of each probe fall strictly inside it, and every
one answered in 0.7–0.9 s: no spike at the boundaries, no gap, nothing
distinguishing the save window from the rest of the run at all.

Recorded because the opposite is the expensive thing to believe by accident. A
user-reported "the page stopped answering after ~30 minutes" landed on the
autosave as its prime suspect — the interval matches exactly (1800 s), and the
reporting client's last synced block was one the server had begun a save at.
Both facts were coincidence; the real cause was an unbounded `fetch` in the
browser client (ADR-0035). Chasing it would have meant lengthening
`--save-interval` or reaching for the journal to "fix" a save that was never
costing anything.

Same run, unrelated and worth its own fix: `~/server-complete.log` carries
154,010 `skipping sample` lines and 432 `dark checkpoint` warnings, all from the
reconcile provider answering archive-depth `eth_getBalance` with
`HTTP 403 … Archive requests require a personal token`. Current-window
reconciliation still passes (8 accounts exact per checkpoint, continuously), so
the cross-provider safety net is up — but its deeper-history half has been dead
since publicnode tightened that policy.

### 5.6 The five-problem round: redeploy, the journal drill, and the snapshot correction (2026-07-28)

Repo at `7fcc392` — PRs #31–#35 (ADR-0036 … ADR-0040) merged and deployed in one
pass. What follows is the recorded output, not a summary of it.

#### The export is wrong, the deployment much less so, and the gap is the whole story

§5.4 recorded that the BigQuery export is "not exact at its own boundary" and
guessed the suspect population was small and boundary-local. Both halves of that
guess were wrong, and `bq show` says why: `crypto_ethereum.balances` is a
**materialized table, "updated daily"** (ethereum-etl, 453,102,032 rows,
`lastModifiedTime` 2026-07-27) with **no block-number column**. It has one
effective height per daily rebuild, and §2.1's gate query merely *assumes* that
height is the previous UTC day's close. The assumption can fail **in either
direction**.

Method for everything below: for each block in a window ending at
B = 25,613,233, fold `debug_traceBlockByNumber` (prestate tracer, diffMode) to
each account's absolute post-state at its last touch ≤ B — that value *is* its
balance at B — then diff against the exported CSV rows. The fold was checked
against `gateway.tenderly.co`'s archive `eth_getBalance(a, B)` on 5 of 5 probed
accounts before being trusted.

| population | measured |
|---|---|
| 600 random **exported rows** vs chain at B (tenderly ∧ blastapi, both must agree; 0 dropped) | **2 wrong → 0.33%**, Wilson 95% CI [0.09%, 1.21%] |
| 1,346,000 accounts trace-touched in (B−20000, B] | **31,200 wrong rows + 14,442 funded-but-absent**, plus 12,004 withdrawal-only recipients |
| 200 random exported rows vs the **live server** at its head | **0 wrong of 200**, CI [0.00%, 1.88%] |
| 150 *known-wrong* window rows vs the live server | **28 still wrong** — 18.7%, CI [13.2%, 25.7%] |
| 100 *known funded-but-absent* accounts vs the live server | **22 still served as `0x0` while funded** — 22.0%, CI [15.0%, 31.1%] |

Wrong-rate by depth of last touch before B does **not** decay to zero: 27.99% at
depth ≤1, 16.28% at (5,10], 12.66% at (50,100], 7.83% at (250,500], 6.62% at
(500,1000], 3.04% across (1000,20000].

Those last three rows are the finding. **The export's error is not the
deployment's error**, and quoting one as the other overstates the live risk by
about an order of magnitude — but the residual is real, and a uniform sample
cannot see it: 0/200 is exactly what ~0.2% of 200.5 M looks like at n=200. One
explanation covers both:

- **Export ahead of B** — the row reflects state *after* B, so the account was
  touched in (B, h]; the ordinary forward replay re-applies absolute post-state
  over exactly that range and it **heals**. This is the ~80% that healed, and a
  rewind is irrelevant to it.
- **Export behind B** — the row reflects state *before* B, so the account changed
  in (h, B]; the forward replay from B+1 never revisits that range and the row is
  wrong **permanently**. This is the ~19–22% residual, and it is precisely and
  only what `--snapshot-rewind` reaches backward for.

Concretely, from the residual sample:
`0x67e11115a4173beda9ce1818de6dd2bbc57f7f80` was served as **0.017472 ETH** when
the chain says **0.000000** — emptied before B, in a range the replay never goes
back to. `0xcd7f7cc5e7d037ef9fc9940e64fd083800518c94` was served as `0x0` while
holding 0.000742 ETH.

Largest single export error found: the beacon deposit contract
`0x00000000219ab540356cbb839cbe05303d7705fa` — export 88,453,361.5383 ETH,
chain 88,593,844.4457 ETH, **+140,482.91 ETH**.

#### The journal recovery drill — proven, not assumed

ADR-0026 shipped `<state>.journal` in 2026-07 and nobody had ever switched
restoring on. ADR-0037 made it the default; this is the drill that justified it,
run on the live box against the real 24.18 GB state file.

```
# graceful stop first, so the base save is known exactly
risepir-rpc mainnet: state saved (shutdown): block 25630477, 24.18 GB in 121.4s (199 MB/s)
risepir-rpc mainnet: state saved; exiting

# restart on the new binary
risepir-rpc mainnet: state loaded in 25.1s — block 25630477, 200819779 accounts, complete set
risepir-rpc mainnet: journal matched the base but had nothing new to replay
State autosave: every 21600s while following (default for --journal-restore on — pass --save-interval to override; 0 disables).

# ... 32 blocks applied, then a deliberate `pkill -9` — no save, no chance to save
HEAD BEFORE KILL: ok 25630509
killed -9 (no graceful save)

# restart
risepir-rpc mainnet: state loaded in 12.3s — block 25630509, 200820858 accounts, complete set
risepir-rpc mainnet: journal replayed: 32 block(s) in 0.177s — resuming at block 25630509 (base was 25630477)
risepir-rpc mainnet: journal: 32 record(s), 318497 bytes since the base save (base state file is 24176139523 bytes)
```

It resumed at the last **applied** block (25,630,509), not the last **saved** one
(25,630,477). The 32 blocks cost **0.177 s** to replay from the journal against
roughly 32 s to re-fetch them from the feed, and the journal that bought it is
**318,497 bytes against a 24,176,139,523-byte state file** — a ~76,000× write
reduction for the same recovery point.

Write volume, the reason any of this matters: at the old unconditional 1800 s
interval the box rewrote 24.18 GB roughly every 32 minutes — ~45 saves/day,
**≈1.1 TB/day**. At the new 21600 s default it is 4 saves/day, **≈97 GB/day**,
plus a journal running about 10 KB/block (~72 MB/day). Recovery got *better* at
the same time, not worse.

#### The correction applied, and what it did not reach

The 20,000-block window's diff produced a 57,646-address list (funded-but-absent
first, then wrong rows by descending error, then withdrawal recipients), fed to
`--hard-refresh`. Its own report:

```
hard-refresh: done at block 25630606 — 57646 checked, 6833 agreed, 50813 skipped
(disagreement or fetch error), 4200 already correct, 2633 corrected,
3105078215573031991516 wei total absolute correction.
```

**2,633 accounts corrected, 3,105.08 ETH of absolute error removed** from the
served set — every one of them written only where `gateway.tenderly.co` and
`eth-mainnet.public.blastapi.io` independently returned the same value at the
same explicit height, and only where that value differed from what was stored.

Verified afterwards against **`ethereum-rpc.publicnode.com`** — a third provider,
neither of the two that produced the corrections — at block 25,630,669: the first
60 addresses of the list (the ones processed before throughput degraded) are
**60/60 byte-exact**.

**What it did not reach, stated plainly.** 50,813 of 57,646 addresses were
skipped, and 67,791 fetch failures were logged, essentially all
`HTTP 429 Too Many Requests` from both keyless providers: `--hard-refresh`
issues two reads per address at a fixed concurrency of 8 and has no backoff, so
against keyless public endpoints it spends most of its budget being refused. It
fails *safely* — no quorum means no write, never a wrong value — and it is
idempotent, so a re-run picks up what was skipped. But as shipped it is
inefficient against exactly the endpoints it defaults to, and that is a real
defect found by deploying it, not a tuning nicety. A sample of the *unprocessed*
remainder still shows 15 of 25 funded-but-absent and 10 of 25 wrong rows
disagreeing with publicnode, which is what an 88% skip rate looks like.

#### `Range` resume works through Caddy, in production

The interesting risk was the reverse proxy, not the handler. Against the public
origin:

```
etag="setup-a29c909422165ae4-25630605"

# matching If-Range
HTTP/2 206
content-range: bytes 100-131/553819345
content-length: 32

# stale If-Range — must NOT splice two regenerations
HTTP/2 200
content-length: 553819345
```

32 bytes instead of 553,819,345, and a stale validator correctly refuses the
range and serves the whole bundle. Two identical range requests returned
byte-identical data. `accept-ranges: bytes` is present on the full response.

The epoch was **unchanged across the whole redeploy** (`a29c909422165ae4`), so
every client holding a cached hint kept it — which is what ADR-0038's cache
exists for.

#### The hint cache, against the real 553.82 MB deployment

Mock cannot answer the question that actually mattered — whether a browser will
accept a **553 MB** IndexedDB write at all, or refuse it on quota. So
`web/test/browser.mjs` was run against the public origin, real Brave, real
complete set:

```
ok    a second visit boots successfully from the cached hint
ok    ...with no body-bearing GET /setup at all — the whole point of caching the hint
      cached reboot: 5050 ms wall clock (navigate → query box visible); page says:
      "Ready in 2.9 s — 553.8 MB of hint held locally (loaded from this browser's
       cache), pinned at finalized block 25630701."
ok    the ready line says the hint came from this browser's cache
ok    a lookup completes after the cached boot
ok    ...and matches the original session's answer for the same address exactly
ok    no Content-Security-Policy violations
PASS: 0 failing checks
```

**341.9 s on the original report, 2.9 s on return** — and zero bytes of hint on
the wire the second time. The quota question is answered: 553.8 MB was accepted,
not refused. The post-cache lookup returning byte-identical answers to the
pre-cache session is the check that matters for correctness, not speed.

#### Health at a glance, publicly

`GET /metrics` and `GET /status` are live on the public origin. First scrape
after the redeploy:

```
risepir_build_info{version="0.1.0",epoch="a29c909422165ae4",mode="complete"} 1
risepir_head_block 25630509
risepir_finalized_block 25630509
risepir_block_lag 0
risepir_store_items 200820858
risepir_store_load_factor 0.7481159940361977
risepir_state_save_configured 1
```

`GET /healthz`'s first line is still byte-for-byte `ok <block>`, now followed by
ADR-0036's three `reconcile_*` additions and ADR-0040's `snapshot_audit=` line.

One gauge worth knowing before reading the dashboard: with a 6 h save interval,
`risepir_state_save_last_*` all read `0` for hours after a restart, because no
save has happened yet in that process. `risepir_state_save_configured 1` is what
distinguishes that from "not configured".

#### An incidental observation, recorded because it looked like a fault and was not

For ~20 minutes mid-redeploy the server's head sat still at 25,630,637 with
`risepir_block_lag 0`. That is not a stall: `publicnode`, `tenderly`, `merkle`
and `drpc` all independently reported `finalized` = 25,630,637 while the public
`latest` was 25,630,729 — **Ethereum's own finality was 92 blocks behind**. This
deployment follows `finalized` by design (ADR-0007), so a finality lag looks
exactly like a stalled head unless you check. `risepir_finalized_block` is the
field that tells the two apart, and it is new in ADR-0039.

### 5.7 Three defects only a live deployment could show (2026-07-29)

A health check of the running box turned up three defects, all fixed, merged
and then verified against the redeployed binary (PRs #38–#40). They share one
cause worth stating first, because it is the reusable lesson: **CI runs the
gates against `mock`**, which has no real latency, no rate limits and no page
traffic. All three were green everywhere CI ran them and broken everywhere they
mattered.

**Starting state, all healthy:** up 1 d 22 h, head = finalized = 25,638,703
(`risepir_block_lag` 0), 201,022,150 items at load 0.749, `GET /mode` = 1,
reconcile 8/8 exact per checkpoint with `risepir_reconcile_consecutive_dark` 0
and zero mismatches ever, 47 saves and `risepir_state_save_failures_total` 0.
The 21 `CRITICAL` lines in the log are all §5.4's catch-up dark-reconcile
window, long since recovered.

**(1) Eight front-end routes were served outside the metrics layer.**
`Router::layer` wraps only the routes registered *before* it is called, and
`router_with_web` attached the static assets *after*. Measured on the live box
before the fix — every request a 200, not one counter moved:

| request | status | counter |
|---|---|---|
| `GET /` | 200 | — none — |
| `GET /status` | 200 | — none — |
| `GET /status.css` | 200 | — none — |
| `GET /status.js` | 200 | — none — |
| `GET /index.html` | 404 | `unmatched` +1 (via the fallback) |

The exposition carried no `route="index"`, `"asset"` or `"status"` sample at
all after two days of serving a public page, so `route_label`'s arms for them
were unreachable code and `/status`'s own request table read zero page loads
for the page being read. After redeploying, five requests produced exactly
`asset` 3, `index` 1, `status` 1.

**(2) `--hard-refresh` was rate-limit-bound, not disagreement-bound**
(ADR-0041). The §5.6 pass reported 50,813 of 57,646 skipped and the log says
why without ambiguity: **all 67,791 fetch-failure lines were HTTP 429**, and
there were **zero** `providers disagree` warnings. Every skip was a fetch that
never landed. Retrying each fetch 4× with per-address-jittered backoff, then
re-running the same 57,646-address file at block 25,638,862:

| run | checked | skipped | rate |
|---|---|---|---|
| before (full pass, block 25,630,606) | 57,646 | 50,813 | **88.1%** |
| after | 1,000 | 80 | 8.0% |
| after | 3,000 | 208 | **6.9%** |

~12.8× better, and *improving* with run length rather than decaying, so it is
not an artifact of unexhausted quota. Per provider after 3,000 addresses:
`blastapi` 50,596 → 208, `tenderly` **17,195 → 0**. The backoff fully absorbs
tenderly's throttling. 1,013 corrections were found in the first 3,000
addresses, against 2,633 in all 57,646 before.

**(3) `web/test/e2e.mjs` crashed against a real deployment instead of
reporting.** The resume section builds its session with `stallTimeoutMs: 1200`
— chosen so its *abort stub* settles fast — then does a real `/answer` through
it. Live answer latency is p50 ≈ 0.37 s with a tail past 1 s (the `/metrics`
histogram that same day: 111 of 114 ≤ 0.5 s, one in (1, 2.5]), so the real
lookup blew the stub's budget. The `await` was unguarded, so the rejection
escaped the module and killed the process *before* the PASS/FAIL summary —
silently discarding 29 already-passing checks, including the real private
lookup:

```
0xfe0c760cbcb9da239b9ba805f0aeaed3be84f65a
  = 462867589957615167 wei (0.462867589957615167 ETH) at block 25638735
```

Because the crash was at the *last* check in the file, nothing after it had
ever run against a live deployment. With the budget restored and the call
guarded, the full gate reports `PASS: 0 failing checks` — confirming no second
live-only defect was hiding behind the first.

**Housekeeping.** The 33.77 GB `(3,4)` rollback file was deleted (§5.4), disk
92 G → 58 G of 246 G. The box was then stopped after a clean SIGINT save at
block 25,638,894 — 24.18 GB in 121.8 s (198 MB/s), `state saved; exiting`. Note
the shutdown probe trap from §5.4 fired again in a harmless form: `pgrep -f
"risepir-rp[c]"` still returned a PID after the server was gone, because the
bracketed pattern does not stop the `gcloud … --command` *wrapper* from
matching itself. `pgrep -af "^\./target/release/risepir-rpc"` is the probe that
answers the question actually being asked.

### 5.8 Re-bootstrapped onto the `xxh3_128` / `RPST3` lineage (2026-07-31)

The migration §4 describes, executed. Cause: the IKPIR pin moved to `0f3b99b`
(f = 64 / corrected Lemma 2), which changed the primitive's item hash
`xxh3_64` → `xxh3_128`. ADR-0042 kept this repo at `fingerprint_bits = 32`, so
**no geometry moved** — and that is exactly what made the state file dangerous
rather than merely stale.

**The refusal fired first, by name, on the production file.** Pointing the new
binary at the existing 24,176,139,523 B state file, before touching anything:

```
risepir-rpc mainnet: loading state (--journal-restore) from /home/admin/risepir-state.bin ...
risepir-rpc mainnet: fatal: loading …: state file rejected: state file is RPST2,
written before the primitive's item hash changed from xxh3_64 to xxh3_128 — every
key hashes to a different bucket now, so these cells no longer describe a filter
this binary can read, and loading them would miss on every lookup (answering 0x0
for accounts that exist). This is not disk corruption, and the geometry is
unchanged, which is exactly why nothing cheaper catches it; move the --state file
aside and re-bootstrap from a fresh snapshot (do not restore from backup, the
file itself is fine)
```

`exit 1`, in under a second, before any of the 24 GB was read. Neither
`STORE_ARITY` nor ADR-0042's `check_geometry_lineage` could have caught this —
both compare geometry, and the geometry was correct.

**A fresh snapshot was taken first**, because replaying from the 2026-07-26
snapshot would have been 36,660 blocks (~10.2 h). New gate query:

```
nonzero_accounts   201059658        # was 200,503,969
snapshot_block     25641938         # was 25,613,233
dataset_head_time  2026-07-29 23:59:59
```

`dataset_head_time` was one daily ETL rebuild behind (not "today" as §2.1
prefers). Proceeded because the `balances` and `blocks` tables agree internally
— `MAX(number)` capped at 25,641,938 *is* the block at that head, so
`snapshot_block` was not the "too high" case §2.1 warns is never safe. The
account count grew by 555,689; `xtask geometry` confirmed the geometry is
**unchanged** at it (67,108,864 buckets, pb 8, cells/slot 22, 23.62 GB), load
0.7469 → 0.7490. Export: 322 shards / 5.86 GB, pulled same-region at
**874.5 MiB/s in 10 s**; bucket and dataset deleted immediately after.

**Measured, end to end** (`--snapshot-rewind` left at its default 2000, so the
snapshot was treated as exact at 25,639,938):

| step | measured |
|---|---|
| snapshot ingest | **451 s** — 201,059,658 rows, 201,059,658 nonzero, **0 zero skipped** (was 734 s in §5.4) |
| PIR setup + first save | state saved at block 25,639,938, **24.18 GB in 115.2 s** |
| bootstrap, start → saved | **12 min 46 s** (03:50:34Z → 04:03:20Z) |
| catch-up replay | 10,816 blocks at a measured **1.72 blocks/s** — not the ~1 s/block the docs assumed |
| replay wall clock | **~1 h 42 min** |
| **total, start → caught up** | **~1 h 55 min** |
| state file | 24,176,139,523 B — byte-identical in size to the `RPST2` file it replaced |
| peak memory | 26 GB of 62; load average 0.12 once following |

**Verified:**

- `head -c 5 ~/risepir-state.bin` → **`RPST3`**.
- Caught up to finalized exactly: deployment head == `finalized` == 25,650,754.
- **`reconcile at block 25650750: 8 account(s) exact vs independent provider`** —
  and `/healthz` `reconcile_consecutive_dark=0`, `reconcile_comparisons_total=8`,
  `reconcile_reservoir_checks_total=2` (ADR-0036's backfill working).
- **Zero** `MISMATCH` / `halted` / `CorruptStoredValue` / `FingerprintAmbiguity` /
  `TableFull` / `panic` in the entire run.
- Public origin: `GET /setup` returns `content-length: 553819345` (the documented
  553.82 MB), `x-risepir-mode: 1`, `accept-ranges: bytes` + ETag.
  `x-risepir-epoch` is **new** (`46226713616e89da`) — the re-bootstrap re-seeded,
  so cached browser hints from the old deployment are correctly invalidated even
  though the geometry did not move, which is precisely the case ADR-0042 noted the
  epoch's *geometry* fold-in cannot see on its own.
- Only 80/443 reachable from outside; `:8545`/`:8645` refused.

**The complete-set patch time, finally measured.** `docs/numbers.md` §7 says
plainly it "has never been measured at the complete set" and extrapolates
~62 ms (55–75 ms). Directly logged here, at the deployment's own K:

| regime | mean | min | max | mean K |
|---|---|---|---|---|
| during catch-up replay | 8.23 ms → 8.81 ms | 2.51 ms | 20.14 ms | 311–326 |
| once following the head | **11.09 ms → 11.11 ms** | 0.66 ms | 29.20 ms | 303–323 |

**~11.1 ms following**, at the same K≈300 the bench table uses — roughly **5–7×
better than the extrapolation**, which means §6's rebuild÷patch ratio at
deployment scale is correspondingly higher than the ~2 × 10⁴ that section
derives. Two windows in each regime agree closely, so this is a stable figure,
not a single lucky sample.

**A third-party regression this round exposed.** The reconciler was dark for
**360 consecutive checkpoints** before its first success. Two distinct causes,
in sequence, and only the second is a problem:

1. While replaying, every checkpoint was *deferred* — ADR-0036 skipping the
   fetch because the applied block was deeper than the reference provider
   serves. Correct behaviour, and it cleared on its own.
2. Once close to the head, checkpoints *attempted* and every fetch returned
   **`HTTP 403: "Archive requests require a personal token"`** from
   `ethereum-rpc.publicnode.com`. Probed directly: its keyless window is now
   **~64–127 blocks** (serves at −64, refuses at −128). Reconcile only began
   succeeding once the deployment was inside that window.

So the keyless reconcile path now depends on the deployment staying within
~100 blocks of the head, which it is only when fully caught up. Any deep
catch-up runs unverified until it converges — loudly, which is the design
(ADR-0027), but the escalation used to blame the provider for the *deferred*
phase; PR #49 fixes that wording. A keyed endpoint would remove the
constraint entirely and is the obvious next step if this recurs.

**Superseded artifacts.** `~/risepir-state.bin.rpst2-20260731` (24.18 GB) and
`~/risepir-state.bin.pre0728` (24.18 GB) are both `RPST2` and therefore
**unloadable by any binary built from this tree** — that is what the version
check guarantees. They are evidence, not a rollback: reverting means reverting
the pin *and* re-bootstrapping anyway. Delete them to reclaim ~48 GB once this
round is trusted, on the same reasoning that reclaimed the 33.77 GB arity-3
file in §5.4.

### 5.9 The public origin moved to demo.risepir.org (2026-08-17)

The domain and static-IP change §3.7 describes — `risepir.org`, registered at
Cloudflare (also the zone's DNS), fronting the reserved address
`136.115.93.177` — was cut over on the live box and verified end to end: a
new Caddyfile, a staging-then-production certificate, a plain restart of the
complete-set server, both browser gates, and the boundary re-probed from
off-box. Steps below noted as re-run independently were re-run from the Mac,
off the VM, not merely captured once on it.

**Timeline (UTC):**

| time | event |
|---|---|
| 08:30:20 | VM `risepir` started (`gcloud compute instances start`) — static IP `136.115.93.177`, no DNS step |
| 08:35:38 | Caddy obtained **staging** cert for `demo.risepir.org` |
| 08:50:08 | server launched in tmux (`mainnet --state ~/risepir-state.bin --web web`) |
| 08:52:02 | state loaded — **113.7 s**, block 25650914, 201,283,514 accounts, complete set |
| 08:52:07 | Caddy obtained **production** cert for `demo.risepir.org` |
| ~09:45–10:05 | both gates and the port probes re-run from the Mac |

**Caddyfile.** The live path is `/etc/caddy/Caddyfile` — confirmed from
`systemctl cat caddy`: `ExecStart=/usr/bin/caddy run --environ --config
/etc/caddy/Caddyfile`. The original file (993 B, single-hostname) was backed
up to `/etc/caddy/Caddyfile.bak-20260817` before editing, then:

```
$ sudo caddy validate --config /etc/caddy/Caddyfile --adapter caddyfile
Valid configuration
```

**This was the file's first real syntax check** — Caddy is not installed on
the dev Mac, so nothing short of the VM itself can validate a Caddyfile
before it goes live.

**Certificates — staging first, then production.** Staging (verified on the
VM):

```
issuer=C = US, O = Let's Encrypt, CN = (STAGING) Baloney Bulgur YE2
subject=CN = demo.risepir.org
notBefore=Aug 17 07:37:02 2026 GMT / notAfter=Nov 15 07:37:01 2026 GMT
```

Production, independently re-verified from the Mac:

```
issuer=C=US, O=Let's Encrypt, CN=YE1
subject=CN=demo.risepir.org
X509v3 Subject Alternative Name: DNS:demo.risepir.org
notBefore=Aug 17 07:53:35 2026 GMT / notAfter=Nov 15 07:53:34 2026 GMT
Verification: OK / Verify return code: 0 (ok)
```

The old DuckDNS name kept its own, separate production certificate, also
independently re-verified:

```
issuer=C=US, O=Let's Encrypt, CN=YE2
subject=CN=private-eth-getbalance.duckdns.org
notBefore=Jul 26 09:20:56 2026 GMT / notAfter=Oct 24 09:20:55 2026 GMT
```

**A Caddy hot reload does not re-issue across a CA change.** After removing
the `acme_ca` staging line and running `systemctl reload caddy`, **zero
issuance attempts appeared in the journal for 10+ minutes**, although the
staging path had issued near-instantly moments before. On-disk storage *is*
correctly split per issuer (`…/acme-staging-v02…-directory/demo.risepir.org`
existed, `…/acme-v02…-directory/demo.risepir.org` did not) — so the cause is
an in-memory certificate cache keyed by hostname that a hot reload does not
re-check for issuer. A single `systemctl restart caddy` triggered production
issuance within **5 seconds**.

**§3.7 step 3 already said "restart", and this round did not follow it** — it
reloaded, and lost ten minutes to a step the runbook had got right the first
time. What is new here is not the instruction but the *reason*, which the
runbook never gave: because Caddy's on-disk storage is split per issuer, a
reader can reasonably assume changing the issuer is picked up like any other
config change, and reload is the reflex for a config change. It is not
picked up, the failure is silent — no error, no attempt, just nothing in the
journal — and it is the step most likely to be misread as "ACME is broken".
Recording the mechanism is what turns that line of the runbook from a style
preference into a requirement.

**The server.**

```
08:50:08Z loading state (--journal-restore) from /home/admin/risepir-state.bin ...
08:52:02Z state loaded in 113.7s — block 25650914, 201283514 accounts, complete set
08:52:02Z journal matched the base but had nothing new to replay
08:52:02Z reconcile: every 30 block(s), 8 sample(s)/checkpoint against https://ethereum-rpc.publicnode.com
```

**113.7 s cold start → serving is a new measurement**: the runbook has never
before timed a plain complete-set *restart* — §5.8 timed a *bootstrap* (a
fresh snapshot ingest plus PIR setup), at 12 min 46 s. It is this fast
because the previous shutdown was clean, so the journal had nothing to
replay.

Origin headers, re-verified from the Mac:

```
GET /mode  → HTTP/2 200, content-length: 1, body byte 0x01   (complete set)
GET /setup → content-length: 553819345          (the documented 553.82 MB)
             x-risepir-mode: 1
             x-risepir-epoch: 46226713616e89da  (unchanged from §5.8 — no re-seed)
             accept-ranges: bytes
             etag: "setup-46226713616e89da-25651035"
             via: 1.1 Caddy
http://demo.risepir.org/ → 308 → https://demo.risepir.org/
```

**Both gates, re-run from the Mac, off the VM, against the new origin.**
`node web/test/browser.mjs https://demo.risepir.org --require-browser`,
driving Chrome for Testing 152.0.7977.42 on the Mac:

```
driving Google Chrome for Testing against https://demo.risepir.org (COMPLETE set)
  ok  the page boots and the private client initialises
  ok  a complete-set account shows an exact wei balance
      0x5555555555555555555555555555555555555555 -> 1345804390675688562 wei
  ok  the answer is labelled with the block it is as of
  ok  the wire panel reports LWE ciphertext and no addresses
  ok  real entropy was drawn in the browser
  ok  two browser queries for the same address send different ciphertext
  ok  an absent address in a complete set shows 0 and explains that absence means zero
  ok  a second visit boots successfully from the cached hint
  ok  ...with no body-bearing GET /setup at all
      cached reboot: 4842 ms wall clock
  ok  no Content-Security-Policy violations
  ok  no uncaught page errors
PASS: 0 failing checks          (16 checks, exit 0)
```

`node web/test/e2e.mjs https://demo.risepir.org`, also re-run from the Mac:

```
connecting to https://demo.risepir.org ...
  mode=COMPLETE pinned=25651035 arity=2 setup=553.82 MB
  ok  the query carried no address-sized plaintext
      0xff3c5c96b3a8e35ca9c67ee1366c66831404b622 = 1538770205383092 wei
      (0.001538770205383092 ETH) at block 25651039
  ok  an absent address is exactly 0x0 for a complete set
  ok  repeated queries for one address send different ciphertext
  ok  repeated queries keep a constant size (no length leak)
  ok  a truncated-then-resumed /setup boots successfully
  ok  the retry carried Range for exactly the missing tail
  ok  ...and If-Range naming the first response's own ETag
PASS: 0 failing checks          (51 checks, exit 0)
```

**The CLI client, driven from the Mac against the public origin** — the shape
`CLAUDE.md` calls "front end + rewind client on THIS machine against a remote
PIR server", where the queried address never leaves the laptop:

```
2026-08-17T10:06:05Z risepir-rpc client: downloading setup bundle from https://demo.risepir.org ...
2026-08-17T10:07:52Z risepir-rpc client: setup downloaded in 107.0s — hint pinned at block 25651035
  PIR server:  https://demo.risepir.org
  JSON-RPC:    http://127.0.0.1:8546   (local — point your wallet here)
  data set:    COMPLETE nonzero-balance set (not-found answers 0x0)

eth_blockNumber                        → 0x1876760 = 25,651,040
eth_getBalance(0x5555…5555, "latest")  → 0x12ad41f68362ac72
                                       = 1345804390675688562 wei
```

**Two independent client implementations agree byte-for-byte.** The browser
gate's wasm client and this native CLI client returned the *same* balance for
the same address — `1345804390675688562` wei. They are built from one Rust
source but do not share a host: one runs in a wasm sandbox reached through
`web/pir.js` and the DOM, the other natively through the JSON-RPC front end.
Agreement across that boundary exercises the rewind path twice by different
routes, and costs nothing to record.

**The web gates need Node >= 22 — a pre-existing gap, invisible in CI.**
Under the VM's pre-installed Node 18.20.4, `e2e.mjs` fails before any network
I/O:

```
SyntaxError: Named export 'CAPACITY_VERDICT' not found. The requested module
'../pir.js' is a CommonJS module, which may not support all module.exports as
named exports.
```

Cause: the repo has **no `package.json` anywhere**, and `web/pir.js` is a
`.js` file carrying 17 ESM `export` statements. Node < 22 has no
module-syntax detection, so it loads that file as CommonJS and the named
imports fail; Node 22 (syntax detection on by default) loads it as ESM and
the gate passes 51/51. This is pre-existing, not introduced by this change,
and invisible in CI because CI's runner image ships a newer Node. Node
22.23.2 was installed on the VM at `~/node-v22.23.2-linux-x64/` from the
official `nodejs.org/dist` tarball, SHA-256 checked against
`SHASUMS256.txt` before extraction.

**The boundary — only 80/443 reachable, re-verified from the Mac.**

```
nc -z -w6 136.115.93.177 80   → OPEN            (positive control)
nc -z -w6 136.115.93.177 443  → OPEN            (positive control)
nc -z -w6 136.115.93.177 8545 → NOT REACHABLE
curl --max-time 8  http://136.115.93.177:8545/  → curl: (28) Connection timed out
curl --max-time 10 http://136.115.93.177:8645/  → curl: (28) Connection timed out (exit 28)
```

The positive control matters: the `:8545`/`:8645` timeouts are meaningful
only because `:443` answered from the same machine at the same time — a
probe that only tried the private ports could not distinguish "the firewall
is correctly closed" from "the whole VM is unreachable".

**The old DuckDNS name — was broken, now fixed.** It resolved to
**a third party's address**, not the VM — precisely the failure mode §3.7 documents
("forgetting it left the origin pointing at whoever now held the old
address"): the VM's old *ephemeral* address, released when the static IP was
attached, now held by someone else. Caddy and the certificate were correct
throughout; only DNS was wrong.

`~/duckdns-update.sh` was run once on the VM (`duckdns: OK`), then
independently re-checked from the Mac:

```
dig +short private-eth-getbalance.duckdns.org A @1.1.1.1 → 136.115.93.177
GET /mode → HTTP/2 200, body byte 0x01, via: 1.1 Caddy
```

Because the IP is now **reserved**, this is a one-time correction, not a
recurring step — there is no dynamic-DNS updater in the start path any more.

#### The deployment was verified while lagging — and the evidence says so

The state file was written 2026-07-31 at block 25650914. The server resumed
there and was at **block 25651035–25651039** during the gates above.
Finalized head at the time was **25,773,642**, captured on the live
`/status` page — **lag ≈ 122,700 blocks (~17 days).** None of the checks
above ran against a current chain state; this is said plainly rather than
letting a clean certificate, clean CSP, and passing gates be misread as the
deployment being current.

**The lag was deliberate, not an oversight.** Catching up would have cost
~122,700 blocks at §5.8's measured 1.72 blocks/s — about **20 h and ~$7 of
VM time** — and every property this cutover needed to demonstrate
(certificate, routing, CSP, same-origin wasm delivery, a byte-exact PIR
answer) is independent of chain height. Every answer above is labelled with
the block it is as of, so the box was verified **while lagging, and the
evidence says so**, rather than implying it was current.

Catch-up would not have converged during this window regardless: both feed
endpoints were refusing throughout —

```
dRPC:      HTTP 408 "Request timeout on the free plan"
merkle.io: HTTP 429
```

— so the server advanced only ~125 blocks in ~70 minutes (~0.03 blocks/s).

`/healthz` at 10:00Z:

```
ok 25651038
reconcile_configured=1
reconcile_checkpoints_total=4
reconcile_consecutive_dark=4
reconcile_deferred_total=4      <- deferred, not failed
reconcile_halted=0
reconcile_comparisons_total=0
snapshot_audit=checked=183 disagreed=2 block=25641938 rate=1.09% ci=[0.30%,3.90%]
```

`deferred == checkpoints` with `halted=0` is the designed behaviour for a
deployment deeper than the reference provider serves (ADR-0036) — the
deferred/failed wording fix in PR #49 is why this reads as deferral, not as
blaming the provider. The `snapshot_audit` rate of 1.09% (CI [0.30%, 3.90%])
is consistent with the already-documented ~0.33% population-wide BigQuery
export gap (ADR-0040), not a new defect.

**The stop.** The anchored pattern from §3.6
(`pkill -INT -f "^\./target/release/risepir-rpc"`), then the save:

```
2026-08-17T10:11:11Z risepir-rpc mainnet: state saved (shutdown): block 25651040, 24.18 GB in 119.5s (202 MB/s)
2026-08-17T10:11:11Z risepir-rpc mainnet: state saved; exiting
```

No `.tmp` was left behind, `head -c 5 ~/risepir-state.bin` still reads
**`RPST3`**, and the file is 24,176,139,523 B — the size it went in at.
`gcloud compute instances stop risepir` then reported `TERMINATED`, with
`risepir-ip` still `IN_USE`: a reserved address stays attached to a stopped
instance, which is the whole point of reserving it.

**The shutdown-probe trap fired a third time — self-inflicted, and the
existing guidance was already right.** §5.4 first recorded it and §5.7 saw it
again; both concluded that `pgrep -af "^\./target/release/risepir-rpc"` is
"the probe that answers the question actually being asked". This round used
the *bracketed* form instead (`pgrep -f "risepir-rp[c]"`), which went on
reporting a live PID after the server had exited — so a wait-for-exit loop
built on it never saw the exit, and the result briefly read as a server
ignoring SIGINT.

Worth stating why the documented form is the robust one, since two rounds of
notes describe the symptom without quite naming the mechanism. Bracketing
only prevents the probe from matching *its own pattern text*. It does nothing
about a **different, unbracketed occurrence of `risepir-rpc` elsewhere in the
same command** — and a stop script necessarily contains one, in its own
`pkill -INT -f "^\./target/release/risepir-rpc"` line. The wrapper shell's
command line therefore contains the literal string the bracketed probe is
looking for. The **`^` anchor**, not the bracket, is what fixes this: the
wrapper's command line begins `bash -c …`, so an anchored pattern cannot
match it, whatever the script quotes further along.

So: use the anchored form §5.4 already prescribes, not a bracketed one. Where
a probe must be certain regardless of what the surrounding script says, one
that does not pattern-match the command line at all removes the question:

```
ps -eo pid,comm,args | grep -E "target/release" | grep -v grep
```

**Cost.** The VM went up at 08:30:20Z and was stopped at ~10:20Z — **1 h
50 min**; at `e2-highmem-8` that is $8.60/day ($0.358/h). The whole
verification round — start, both certificate issuances, both gates, the CLI
client, the DuckDNS fix, and the stop — cost roughly **$0.66**, against the
~$7 a full catch-up alone would have added.

## 6. Who does what, explicitly

| step | who | needs |
|---|---|---|
| build (`cargo build --release`) | either of us | nothing — `bao-ninh-orochi/IKPIR` is public now |
| §1 partial demo, end to end | either of us (already done, §5) | nothing |
| §2.1 gate query + export | ~~**you**~~ — **done 2026-07-26**, driven from this environment after `gcloud services enable bigquery.googleapis.com` (the project already had billing, so no separate Google-account step was needed after all) | BigQuery + a GCS bucket on the same project |
| §2.2 complete-mainnet run | **done 2026-07-26** — live on the 64 GB box (§5.3) | the shards + the two gate numbers |
| wallet demo | either of us | §1 or §2 running |
