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

Restarts are cheap: with `--state`, startup is a file load (bit-identical PIR
parameters — previously bootstrapped clients stay valid) plus the catch-up replay
since the save. While following, the loop rewrites the file every
`--save-interval` seconds (default 1800, `0` disables — ADR-0025), so that
replay is bounded by the interval: an ungraceful kill costs minutes of catch-up,
not the whole uptime. `Ctrl-C` still saves the exact final state before exiting;
each save logs a `state saved: block …, … GB in …s` completion line.

Beside `--state` there is always a `<state>.journal` sidecar (ADR-0026, once a
first full save exists): one small per-block delta, appended and fsynced as
each block applies, rotated to a fresh file bound to the new digest right
after every save. It is *written* unconditionally; *restoring* from it needs
`--journal-restore` (default off). Off, a restart only *scans* it and logs
`journal intact: N records to block X (--journal-restore to use)` — a
soak signal, not a decision — then loads and replays exactly as above,
unaffected. On, the restart replays the journal onto the loaded state before
serving starts and logs `journal replayed: N block(s) in T s — resuming at
block X (base was B)`, so a kill -9 between saves costs the journal's replay
time (well under a second per block) instead of a network catch-up. The
payoff configuration is a long `--save-interval` (hours) once the report line
has looked healthy for a while, plus `--journal-restore` — recovery to within
seconds of the last applied block at a small fraction of the disk-write cost
a short interval alone would need.

Journal-writing failures never risk correctness, only durability: a bad
rotation logs a `WARNING` and leaves journaling off until the next successful
save retries it; a continuity gap (should one ever occur) disables journaling
for the rest of that run. Either way the periodic full save keeps happening
on schedule — the worst case is falling back to the previous `--save-interval`
behavior, never a wrong answer at replay time (see ADR-0026's two failure
classes).

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
   friction): install rustup + git, then give the box read access to *both*
   private repos. **Not per-repo deploy keys** — GitHub allows a deploy key on
   only one repository, and two repos are needed (this one plus the pinned
   IKPIR dep). Instead: `ssh-keygen` on the box and add the public key as an
   **account SSH key** (github.com/settings/keys), plus
   `git config --global url."git@github.com:".insteadOf "https://github.com/"`
   — required because cargo fetches the IKPIR dep by its https URL, and the
   rewrite routes that fetch through the SSH key. Then clone via SSH and
   `cargo build --release -p risepir-rpc`. On a 2 GB instance add swap first
   (`fallocate -l 4G /swapfile …`) or build once on a larger spot box and copy
   the binary (same arch).
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
gcloud init                    # browser login; create project e.g. "risepir-poc"; pick us-central1-a
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
gcloud billing projects link risepir-poc --billing-account=<ACCOUNT_ID>
```

(Older SDKs spell it `gcloud beta billing …`. Empty list ⇒ the trial was never
activated — do that first. A permission error on linking ⇒ the billing account
belongs to your organization and restricts outside projects — either link via
the console as the org admin, or Manage Resources → select the project →
Migrate it into the organization, then link.) Now:

```bash
gcloud config set project risepir-poc
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
rustup, add the VM's `ssh-keygen` public key as an **account** SSH key
(github.com/settings/keys — see §3.5 for why per-repo deploy keys do not work
here) with the https→SSH `insteadOf` rewrite, clone, `cargo build --release
-p risepir-rpc`, run in `tmux` with `--state`. The instance-create warning
about disk size vs 10 GB image size is expected and harmless — Debian grows
the root partition on first boot (`df -h /` shows the full disk). Pull the snapshot shards straight
from the export bucket: `gcloud storage cp 'gs://<your-bucket>/balances-*.csv.gz' .`
(if the VM's default service account lacks bucket read, the two-minute fix is
`gcloud auth login` on the VM and retry).

**Verified on GCP, 2026-07-19** (project `risepir-poc`, `e2-medium`/Debian 12 in
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
`gcloud billing projects describe risepir-poc` / the console's Billing page shows
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
**<https://private-eth-getbalance.duckdns.org>** — one hostname serving both
the page and the PIR transport, which is what ADR-0019's same-origin
`connect-src 'self'` CSP requires. Every command below was executed as written.

Nothing else is exposed: the firewall opens **only 80/443**, both listeners stay
on `127.0.0.1`, and Caddy has no route to `:8545`.

**1. A free name, from a dynamic-DNS provider.** DuckDNS needs no registration
(OAuth sign-in) and gives 5 subdomains. Because the GCP external IP changes
across stop/start, the record is refreshed *from the VM*, where an empty `ip=`
makes DuckDNS use the request's source address:

```bash
umask 077; printf '%s' '<token>' > ~/.duckdns-token       # 0600, never in git
cat > ~/duckdns-update.sh <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
token=$(cat "$HOME/.duckdns-token")
curl -s "https://www.duckdns.org/update?domains=private-eth-getbalance&token=${token}&ip="
EOF
chmod +x ~/duckdns-update.sh && ~/duckdns-update.sh    # prints OK
```

Run it after every `instances start`, before Caddy needs the name. DuckDNS
serves a 60 s TTL, so the name follows the VM within a minute. This is why no
static IP is reserved: a reserved address bills ~$3.60/mo *even while the VM is
stopped* (attached-and-reserved counts as in use), and dynamic DNS is free.

**2. Firewall — 80/443 only, never the listeners.**

```bash
gcloud compute firewall-rules create risepir-web \
  --allow=tcp:80,tcp:443 --target-tags=risepir --source-ranges=0.0.0.0/0
```

**3. Caddy, staging first.** Production ACME issuance is rate-limited and a
misconfigured loop can lock you out for up to a week, so validate the whole
path — DNS, `:80` reachability, proxy — against the staging CA, which is not
rate-limited (browsers will warn; expected). Then remove the `acme_ca` line and
restart for the real certificate. `ops/caddy/Caddyfile` is the deployed config.

**4. The server, as usual.** Caddy 502s until it is up:

```bash
tmux new-session -d -s risepir "cd ~/private-ETH-getBalance && exec \
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

Zero cash beyond the VM hours: DuckDNS, Let's Encrypt and Caddy are free, no
static IP is reserved, and a handful of visitors is well under a gigabyte of
egress.

It is sized for **a link sent to a few colleagues**, not for public traffic.
Cold visitors now share a single cached `/setup` encode and get a refcounted
clone of it rather than one encode and one buffer each (ADR-0028), so
concurrent bootstraps cost bandwidth rather than multiplying memory — the
per-route `SETUP_MAX_CONCURRENT = 2` cap that used to sit here is gone, having
been measured not to bound what it claimed (tower released its permit when the
handler returned, not when the transfer finished). What remains undefended is
unchanged and is the part that matters: there is **no rate limiting at all**,
and the egress of a large `/setup` bundle per cold visitor is still entirely
real (threat model §3 names volumetric DoS as undefended) — **830.73 MB**
today, since this origin has not been re-bootstrapped since ADR-0034 moved
the deployed geometry to `(arity 2, bucket_size 4)`; a fresh bootstrap cuts
that to **553.82 MB**. `/setup` behind a CDN plus per-IP quotas is roadmap
C5/C3 — do that before sharing the link wider.

**Certificate renewal needs the VM up occasionally.** Caddy renews from ~30 days
before expiry over `:80`. Since this VM is stopped between demos, a gap longer
than that window lets the certificate lapse and visitors get a hard TLS error;
starting the VM for any demo inside the window fixes it automatically.

**The trust chain grew, and this is the price of the free name.** Whoever holds
the DuckDNS token can repoint the hostname at their own machine, obtain a valid
certificate for it (DNS control is all Let's Encrypt checks), and serve a
modified wasm client under this name — and so can DuckDNS itself. That is the
same category as ADR-0019's disclosed code-delivery trust, one party wider. See
threat model §4.2 and §8.

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
> the `(arity 3, bucket_size 4)` bootstrap **as it actually happened**, and is
> **still what the live server is serving today**: the deployed geometry has
> since moved to `(arity 2, bucket_size 4)` at a higher target load, but that
> needs an operator to move `~/risepir-state.bin` aside and re-run
> `~/bootstrap-complete.sh` (~33 min at the complete set), and that
> re-bootstrap has not happened yet. A fresh bootstrap on the current code
> computes to 23.62 GB / load 0.7469 / a 553.82 MB hint / ~1.11 GB in the
> browser (§2.3, §1.5, ADR-0034) — those are the numbers a future reader
> should size against; they are not yet what this section's live evidence
> shows.

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

## 6. Who does what, explicitly

| step | who | needs |
|---|---|---|
| build (`cargo build --release`) | either of us | access to the pinned `bao-ninh-orochi/IKPIR` repo |
| §1 partial demo, end to end | either of us (already done, §5) | nothing |
| §2.1 gate query + export | ~~**you**~~ — **done 2026-07-26**, driven from this environment after `gcloud services enable bigquery.googleapis.com` (the project already had billing, so no separate Google-account step was needed after all) | BigQuery + a GCS bucket on the same project |
| §2.2 complete-mainnet run | **done 2026-07-26** — live on the 64 GB box (§5.3) | the shards + the two gate numbers |
| wallet demo | either of us | §1 or §2 running |
