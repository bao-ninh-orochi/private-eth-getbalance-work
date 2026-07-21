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
# live mainnet, partial set, 49 MB first load (see the table below)
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
| 250,000 | 23 MB | ~46 MB | ~1.5 h |
| 500,000 | 35 MB | ~70 MB | ~3 h |
| **1,000,000** | **49 MB** | **~99 MB** | **~5 h** |
| 4,000,000 (default) | 99 MB | ~198 MB | ~1 day |

Client compute is not the constraint: a full lookup is **10 ms** at 1 M accounts
(three segments, single-threaded wasm, no SIMD), and expanding `A` from its seed
at startup is another ~0.2 s. At the complete ~100 M-account mainnet set the hint
would be **588 MB** — past what a web page should ask for, which is where
`risepir-rpc client` on a real machine takes over.

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
node web/test/e2e.mjs     http://127.0.0.1:8645   # protocol, in a real wasm host
node web/test/browser.mjs http://127.0.0.1:8645   # the page, in headless Chromium
```

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
# back up (server running is fine — copy the *state* file, never the .tmp):
gcloud --quiet compute ssh risepir --command='cp ~/risepir-state.bin ~/risepir-state.bak'
# restore = stop server, put the backup in place, start; the server replays
# saved_block+1..finalized from the feed — the backup's age only costs
# replay time (~1–2 s/block), never correctness:
gcloud --quiet compute ssh risepir --command='cp ~/risepir-state.bak ~/risepir-state.bin'
```

A corrupt file (including a single flipped bit — the checksum catches what
structural checks cannot) is **rejected at load**; partial mode then
re-bootstraps empty, loss-free, while a complete-set deployment restores
the backup or re-runs the snapshot bootstrap. Legacy `RPST1` files load
with a warning and upgrade to `RPST2` on their next save.

**TLS (required the moment either port leaves localhost):** plaintext HTTP
means anyone on-path can *be* the operator (threat model §4.2), and the
browser front end additionally needs TLS for a non-localhost origin. Don't
teach the binary TLS — put a reverse proxy in front and keep the listeners
loopback-only:

```bash
sudo apt-get install -y caddy       # auto-provisions Let's Encrypt
# /etc/caddy/Caddyfile — replace the hostname; DNS must already point here:
#   pir.example.com {
#       reverse_proxy 127.0.0.1:8645
#   }
sudo systemctl reload caddy
```

Serve **only** the PIR port (`:8645`) this way — it is what remote clients
and the browser front end need. `:8545` stays loopback/SSH-tunnel: it
answers *plaintext account queries* by design, so exposing it publicly
hands every visitor's queried address to the network (ADR-0012's warning,
one layer up).

**Cost hygiene:** `gcloud compute instances stop risepir` when idle (only the
disk's ~$4/mo keeps billing against credit); `…delete` to zero it;
`gcloud billing projects describe risepir-poc` / the console's Billing page shows
credit burn-down. After the credit: switch the same VM to Spot
(~$18–30/mo for 16 GB) or move to Oracle's free tier.

### Which cloud, if the goal is spending nothing

| option | complete-set (16–24 GB) cost | notes |
|---|---|---|
| AWS on-demand (`r7g.large`) | ≈ $77/mo + $2.4 EBS | most convenient for you (aws-cli ready); no free tier at this RAM |
| AWS spot (`r7g.large`) | ≈ $23–35/mo | interruptions are cheap here (state file + catch-up) |
| GCP (`e2-highmem-2`) + new-account $300 credit | ≈ $0 for ~3–4 months | you need a GCP account for the BigQuery export anyway; same-region GCS→VM snapshot copy is free |
| Oracle Cloud Always Free (4 OCPU/24 GB Ampere) | $0 indefinitely | genuinely free; signup/capacity availability is famously flaky |
| this MacBook (16 GB) | $0 | complete set fits only marginally (~13–16 GB working set) — fine for partial, risky for complete |

The partial demo runs on anything ≥2 GB, including AWS free-tier-class instances.

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

## 6. Who does what, explicitly

| step | who | needs |
|---|---|---|
| build (`cargo build --release`) | either of us | access to the pinned `bao-ninh-orochi/IKPIR` repo |
| §1 partial demo, end to end | either of us (already done, §5) | nothing |
| §2.1 gate query + export | **you** (Google account; I have no GCP access from this environment) | BigQuery sandbox; billing (or $300 credit) for the export step only |
| §2.2 complete-mainnet run | either of us, on the 16–24 GB box | the shards + the two gate numbers |
| wallet demo | either of us | §1 or §2 running |
