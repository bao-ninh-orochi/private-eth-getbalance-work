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
  `042d868` (`perf/optimized` tip). Needs read access to that private repo;
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
  running with `--web web`; both adapt to `GET /mode`; neither needs npm.
- `ikpir-common` is inherited `default-features = false`; every crate re-enables
  the rayon kernels via its own default-on `parallel` feature. A new crate
  depending on it needs that forwarding feature or it silently builds the scalar
  kernels the numbers were not measured against (ADR-0019).

## Git conventions

- Push directly to `main` (user's private repo — no fork, no PR). Sign every
  commit; if signing hangs: `ssh-add --apple-use-keychain`. **No AI-attribution
  trailers or footers.**

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
`--partial-capacity 1000000`, 588 MB at the complete mainnet set (which is
where the CLI `client` takes over). Its residual trust — you trust whoever
serves the page — is stated on the page itself, not just in the ADR.

Feed = dRPC keyless (traces); reconcile = publicnode keyless (independent
operator). `"latest"` = **finalized**, ~13 min behind the public head, by design
(ADR-0007) — conformance checks must compare at an explicit height, never at
"latest"-vs-"latest".

## The live GCP deployment

Project **`risepir-poc`**, VM **`risepir`** (`e2-medium`, Debian 12,
`us-central1-a`); repo at `~/private-ETH-getBalance`, server runs in tmux
session `risepir` with `--state ~/risepir-state.bin`, logs at `~/server.log`.
The Mac's `gcloud` + `gh` are authenticated; the VM is drivable
non-interactively:

```bash
gcloud --quiet compute ssh risepir --command='...'
# resume after a stop:
gcloud compute instances start risepir
gcloud --quiet compute ssh risepir --command='tmux new-session -d -s risepir \
  "cd ~/private-ETH-getBalance && exec ./target/release/risepir-rpc mainnet --partial \
   --state ~/risepir-state.bin >> ~/server.log 2>&1"'
```

The `exec` is load-bearing: it makes the binary *be* the tmux pane process, so
signalling it never involves a wrapper shell. With a state file present the
server loads it and replays the missed blocks; without one, partial mode just
re-bootstraps empty at the current finalized head — loss-free. (The external IP
changes across stop/start; the tunnel doesn't care:
`gcloud compute ssh risepir -- -L 8545:localhost:8545`.)

Stopping the meter — in this order:

```bash
gcloud --quiet compute ssh risepir \
  --command='pkill -INT -f "^\./target/release/risepir-rpc" && sleep 20 && tail -1 ~/server.log'
#   → wait for "state saved; exiting"
gcloud compute instances stop risepir
```

The **anchored** pattern matters: a broad `pkill -f risepir-rpc` also kills the
tmux wrapper shell, tmux then SIGHUPs the pane group, and the server dies
mid-save with a 0-byte `.tmp` (this happened on 2026-07-19; harmless in partial
mode, but a complete-set deployment would lose a multi-hour bootstrap's fast
restart). When checking from `gcloud … --command`, bracket the pattern
(`pgrep -f "risepir-rp[c]"`) or the probe matches its own ssh wrapper. VM SSH
key and the GitHub account key `risepir-gcp-vm` are already set up; the VM
burns ~$0.80/day of trial credit while running, ~$4/mo stopped (disk only).
