# CLAUDE.md — operating manual for this repo

Proof-of-concept **private `eth_getBalance` over RisePIR** (LWE keyword-PIR over a
Segmented Cuckoo Filter): a server follows Ethereum mainnet and answers balance
queries without learning which account was asked. It **works against real mainnet
today** and there is a **live GCP deployment** (below).

## Read before changing anything

1. [`docs/plan.md`](docs/plan.md) — authoritative spec.
2. [`docs/deploy.md`](docs/deploy.md) — the runbook + all recorded live evidence.
3. [`docs/adr/README.md`](docs/adr/README.md) — every decision with rationale;
   **reasoned deviation is welcome, silent deviation is the failure mode** — new
   decisions get a new ADR.
4. [`docs/HANDOFF.md`](docs/HANDOFF.md) — what is left (short version: the
   user-run BigQuery export upgrades the partial demo to the complete set).
5. [`docs/threat-model.md`](docs/threat-model.md) — the adversary definitions;
   a change that moves a security boundary updates it in the same commit.

## The binding rules

- **Never return a wrong answer.** Erroring is fine; labelled-stale is fine; a
  silently wrong balance is total failure. Every `NotFound`/`DecodeFailed`/
  `FingerprintAmbiguity`/`CorruptStoredValue`/strict-partial path exists for
  this. When in doubt, fail loudly.
- **Partial mode never answers `0x0` for an untracked account** and never
  applies a withdrawal credit to one (absence only means zero for a *complete*
  set — ADR-0015/0017; the completeness flag is served via `GET /mode`, never
  guessed).
- **Never hardcode `plaintext_bits` or geometry** — derive via
  `risepir_proto::Geometry`.
- **Validate every length before allocating** — the server ingests
  attacker-controlled blobs; malformed input gives a clean error, never a panic.
- Store writes go through the verified fp ∧ `key_tag` scan
  (`risepir-server/src/verified.rs`, ADR-0017) — never call the store's
  key-addressed `update`/`delete` directly (fp-only first-match corrupts
  colliding entries).

## Build & test

- The PIR primitive is a **pinned git dep**: `bao-ninh-orochi/IKPIR` @
  `3d60fa7` (`perf/optimized` tip, 2026-07-21). Needs read access to that private repo;
  `.cargo/config.toml` sets `git-fetch-with-cli` + `target-cpu=native`. The
  local checkout at `../CANS2026/RisePIR` drifts — read it for API signatures,
  **never** path-dep it.
- Gates, in escalating strength: `cargo test --workspace` (fast, run always) →
  `cargo run -p xtask --release -- conformance` (byte-exact vs ground truth) →
  `cargo test -p risepir-feed --release -- --ignored` (live: trace parsing vs an
  independent provider) → a `mainnet --partial` smoke run (deploy.md §1). Run
  the live ones after touching the feed or the apply path. Report real output.
- Touching the browser client (`crates/risepir-wasm`, `web/`) adds two more:
  `node web/test/e2e.mjs <pir-url>` (real wasm host) and
  `node web/test/browser.mjs <pir-url>` (headless Chromium — it is the only
  thing that can see a CSP, and it has already caught one). Both need a server
  running with `--web web`; both adapt to `GET /mode`; neither needs npm. CI
  now runs both against `mock` on every PR (the `browser` job): a runner with
  no browser fails loudly (`--require-browser`) rather than silently skipping.
  Still run them locally against real mainnet after touching the feed or the
  apply path — CI's mock server does not exercise that.
- `ikpir-common` is inherited `default-features = false`; every crate re-enables
  the rayon kernels via its own default-on `parallel` feature. A new crate
  depending on it needs that forwarding feature or it silently builds the scalar
  kernels the numbers were not measured against (ADR-0019).
- CI (`.github/workflows/`, ADR-0021) enforces `cargo clippy --workspace
  --all-targets -- -D warnings` + the tests on every push, conformance and the
  browser gate (mock mode, real headless Chromium) on PRs, and runs the live
  gate plus the `fuzz/` targets nightly. It fetches the private IKPIR dep via
  the `IKPIR_TOKEN` secret (fine-grained PAT, IKPIR only, Contents read-only —
  deploy keys are disabled on that repo). `cargo fmt` is not yet a gate — no
  mass reformat until the in-flight branches land.

## Git conventions

- Branch → PR → self-merge into `main` (the `PGR-###` rules in the global guide):
  cut a `type/slug` branch off a synced `main`, open a PR against `origin`'s
  `main`, get CI green, then squash-merge and delete the branch
  (`gh pr merge <#> --squash --delete-branch`). **No fork** — `origin` is this
  repo; there is no `upstream`. `main` is protected by convention (no direct
  pushes; server-side branch protection needs GitHub Pro for a private repo — see
  PGR-007). Sign every commit; if signing hangs: `ssh-add --apple-use-keychain`.
  **No AI-attribution trailers or footers.**

## The binary (`risepir-rpc`)

```
mock                          synthetic demo, no network
mainnet --partial             live mainnet, empty-start honest demo
mainnet --snapshot … --snapshot-block … --snapshot-accounts … --state …
                              complete set (shards from deploy.md §2.1)
mainnet/mock --bind 0.0.0.0   expose listeners (PIR :8645, JSON-RPC :8545)
client --pir-url http://host:8645
                              front end + rewind client on THIS machine against
                              a remote PIR server (address never leaves it)
mock/mainnet --web web        also serve the browser front end on the PIR port
                              (ADR-0019); build it first with
                              `cargo run -p xtask --release -- web`
```

The browser front end (`web/`, `crates/risepir-wasm`) runs the *same* rewind
client compiled to wasm **in the page**, so the address never leaves the
browser. Same origin as the PIR transport on purpose (no CORS, no mixed
content, `connect-src 'self'` CSP). Assets are read once at startup — restart
after editing `web/*`. First load is the whole product constraint: 49 MB at
`--partial-capacity 1000000`, but **830.73 MB at the real complete mainnet set**
(200,503,969 accounts, measured 2026-07-26 — the "588 MB" once quoted here was
computed against an assumed ~130 M). A complete-set client also holds **1.66 GB**
resident once `A` is expanded. That is where the CLI `client` takes over. Its
residual trust — you trust whoever serves the page — is stated on the page
itself, not just in the ADR.

Feed = dRPC keyless (traces); reconcile = publicnode keyless (independent
operator). `"latest"` = **finalized**, ~13 min behind the public head, by design
(ADR-0007) — conformance checks must compare at an explicit height, never at
"latest"-vs-"latest".

## The live GCP deployment

Project **`risepir-poc`**, VM **`risepir`** (**`e2-highmem-8`, 8 vCPU / 64 GB,
250 GB disk**, Debian 12, `us-central1-a`); repo at `~/private-ETH-getBalance`,
server runs in tmux session `risepir` with `--state ~/risepir-state.bin`, logs
at `~/server-complete.log`. The Mac's `gcloud` + `gh` are authenticated; the VM
is drivable non-interactively.

Since **2026-07-26 it serves the COMPLETE mainnet set** — all 200,503,969
nonzero accounts, `GET /mode` = 1 — not the partial demo. That is what the
64 GB machine is for: the server DB alone is 35.43 GB (ADR-0023). It costs
**~$8.60/day running**, so stop it when idle.

It is **public** at <https://private-eth-getbalance.duckdns.org> (Caddy + Let's
Encrypt in front of a loopback-only `:8645`; deploy.md §3.7). Only 80/443 are
open — `:8545` and `:8645` are never reachable from outside (re-verified
2026-07-26).

```bash
gcloud --quiet compute ssh risepir --command='...'
# resume after a stop — the DNS refresh comes first, the IP just changed:
gcloud compute instances start risepir
gcloud --quiet compute ssh risepir --command='~/duckdns-update.sh'
# normal restart: the 36 GB state file is loaded, then missed blocks replay
gcloud --quiet compute ssh risepir --command='tmux new-session -d -s risepir \
  "cd ~/private-ETH-getBalance && exec ./target/release/risepir-rpc mainnet \
   --state ~/risepir-state.bin --web web >> ~/server-complete.log 2>&1"'
```

While following, the server rewrites the state file every 30 min by default
(`--save-interval <secs>`, `0` disables — ADR-0025), so a crash or missed
Ctrl-C replays at most the last interval, not the whole uptime. Every save
logs a `state saved: block …, … GB in …s` completion line.

Beside it sits `<state>.journal` (ADR-0026): one small per-block delta,
appended and fsynced as each block applies, rotated to a fresh file at every
save. Always written once a first save exists. Restoring from it needs
`--journal-restore` (default off) — off, a restart only scans and reports it
(`journal intact: N records to block X`), the soak signal before trusting it;
on, a restart replays it and resumes above the last save's height instead of
at it. The payoff is a long `--save-interval` (hours) once that report has
looked healthy for a while, recovering to seconds instead of minutes at a
fraction of the disk-write cost.

The `exec` is load-bearing: it makes the binary *be* the tmux pane process, so
signalling it never involves a wrapper shell. `--web web` is what serves the
browser front end at all.

**A state file present means `--snapshot` is silently ignored** (`mainnet.rs`
prints a note and loads the file). That is the trap to know: leaving the old
*partial* state file in place would have brought the server back up in PARTIAL
mode while every flag on the command line said complete. Re-bootstrapping from
the snapshot means moving the state file aside first — and at the complete set
that costs a full **~33 min** (12 min ingest + 21 min PIR setup), so prefer the
state file. `~/bootstrap-complete.sh` on the VM re-runs the full bootstrap.

(The external IP changes across stop/start — hence `duckdns-update.sh`, whose
empty `ip=` makes DuckDNS take the request's source address. An SSH tunnel
doesn't care either: `gcloud compute ssh risepir -- -L 8545:localhost:8545`.)

Stopping the meter — in this order:

```bash
gcloud --quiet compute ssh risepir \
  --command='pkill -INT -f "^\./target/release/risepir-rpc" && sleep 90 && tail -1 ~/server-complete.log'
#   → wait for "state saved; exiting" — at the complete set this writes 36 GB,
#     so allow well over the 20 s that sufficed in partial mode
gcloud compute instances stop risepir
```

The **anchored** pattern matters: a broad `pkill -f risepir-rpc` also kills the
tmux wrapper shell, tmux then SIGHUPs the pane group, and the server dies
mid-save with a 0-byte `.tmp` (this happened on 2026-07-19; harmless in partial
mode, but **now genuinely expensive** — a complete-set re-bootstrap is ~33 min
of CPU plus the catch-up replay). When checking from `gcloud … --command`,
bracket the pattern (`pgrep -f "risepir-rp[c]"`) or the probe matches its own
ssh wrapper. VM SSH key and the GitHub account key `risepir-gcp-vm` are already
set up; at `e2-highmem-8` the VM burns **~$8.60/day** while running (up from
~$0.80/day as an `e2-medium`), ~$10/mo stopped (250 GB disk only).
