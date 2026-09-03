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
6. [`docs/deployment-numbers.md`](docs/deployment-numbers.md) — the measured
   deployment numbers.

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

- The PIR primitive is a **pinned git dep**: `bao-ninh-orochi/IKPIR` — the
  URL is unchanged, but the pin moved from `rev = "0f3b99b"` to `tag =
  "v0.1.0-perf"`, an annotated, signed tag on the `perf/optimized` tip
  (commit `adecd9c`): `0f3b99b` (the f=64 / corrected-Lemma-2 merge,
  2026-07-31) merged with `orochi-network/IKPIR` main, so a strict superset.
  The `crates/` tree at the tag is bit-identical to `0f3b99b` — no kernel, no
  hash lineage (`xxh3_128`/`RPST3`), no PIR geometry moved, so ADR-0042's
  finding that the bump left `fingerprint_bits = 32` untouched still holds,
  every number measured against `0f3b99b` is still valid, and state files a
  `0f3b99b` build wrote still load. The tag is immutable; a future perf
  revision gets a new tag. `bao-ninh-orochi/IKPIR` is now **public**, so
  fetching it needs no credential. It stays a personal fork rather than
  `orochi-network/IKPIR` itself because the org's `main` can't serve this
  dependency — its `ikpir-common` declares no `[features]` section at all
  (no `parallel` feature) and has neither `backend/gemm.rs` nor
  `backend/prg.rs`. `.cargo/config.toml` sets `git-fetch-with-cli` +
  `target-cpu=native`. The local checkout at `../CANS2026/RisePIR` drifts —
  read it for API signatures, **never** path-dep it. The pin has since moved
  again, to `tag = "v0.2.0-perf"` (commit
  `d91c75fb807d25807104c29b1931f846f007379a`, two commits past `adecd9c`):
  IKPIR's LWE error sampler switched from a rounded continuous Gaussian
  (Box–Muller) to a true discrete Gaussian `D_σ`, same σ = 6.4, same public
  API. No kernel, hash lineage, or geometry moved with it, so every claim
  above still holds for the *current* tag too, except literal bit-identity
  to `0f3b99b`, which the sampler diff breaks; see ADR-0046.
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
  gate plus the `fuzz/` targets nightly. `bao-ninh-orochi/IKPIR` is public
  now, so CI fetches it with no credential — the old `IKPIR_TOKEN` secret and
  its `insteadOf` URL rewrite are gone. **`cargo fmt --all -- --check` is a
  gate** as of 2026-07-31, running first in the `clippy + tests` job — the
  one-off mechanical reformat it was waiting on has landed, and that commit is
  in `.git-blame-ignore-revs` (run `git config blame.ignoreRevsFile
  .git-blame-ignore-revs` once per clone; GitHub honours it automatically).

## Git conventions

- **Fork-based, and merged by someone else — the flip from before.** This
  repo moved under `orochi-network`, so it now runs on the ONR rules in the
  global guide, not the personal-repo (PGR) ones this section used to
  encode. `origin` is a personal fork,
  `bao-ninh-orochi/private-eth-getbalance-work` (the `-work` suffix only
  because a case-insensitive collision with the pre-existing
  `bao-ninh-orochi/private-ETH-getBalance` ruled out the plain name);
  `upstream` is `orochi-network/private-eth-getbalance`. PRs are opened
  **from the fork branch against `upstream`'s default branch** — this is
  also the concrete permission reality, not a style choice: on
  `orochi-network/private-eth-getbalance` the author has `push:false` /
  `pull:true` / `triage:true` as a plain org member, so there is no branch
  creation and no direct push on `upstream` itself. Forking is the only way
  in.
- **The author never self-merges — the single most important inversion.**
  Review and merge belong to a separate reviewer agent, `on-unknown-fish`,
  on its own account and its own tooling; the author requests its review and
  answers every comment, but the merge button is never theirs to press.
- Sync `main` from `upstream` before cutting a branch — confirm `main ==
  origin/main == upstream/main` — then cut a `type/slug` branch (`feat/`,
  `fix/`, `docs/`, `chore/`) off it. One logical change per branch; atomic
  commits whose messages explain *why*, not just what.
- Open the PR as a **draft** while work is in progress; mark it ready only
  once it is implementation-complete **and** CI is green — both conditions,
  not either. CI green is required before anything merges, full stop —
  never around it.
- Sign every commit (SSH: `gpg.format ssh`, `commit.gpgsign true`); if
  signing hangs: `ssh-add --apple-use-keychain`. Never push unsigned.
- **No AI-attribution trailers or footers anywhere on GitHub** — no
  `Co-Authored-By: Claude` trailer, no "Generated with Claude Code" footer,
  in PR bodies, PR comments, or issue comments.
- `gh`, never `curl`, for every GitHub operation.
- Tracking lives on ONKaban
  (<https://github.com/orgs/orochi-network/projects/16>) for org issues —
  assign yourself and move the card as the PR progresses; field IDs and the
  fuller workflow are in the global guide, not repeated here.

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
probe --pir-url <url> --queries-csv <p> --blocks-csv <p>
                              client-side measurement campaign against a live
                              deployment: one long-lived product session (one
                              /setup, never GC'd), per-query latency broken into
                              build / wire / server-reported answer / decode's
                              four ADR-0003 rewind steps / residual, bytes each
                              way, per-fetch delta cost, hint size, client RSS —
                              plus each answer compared byte-exactly against
                              --confirm-url at the SAME explicit block height.
                              The budget closes by construction (residual is the
                              subtraction, never distributed). --resolve
                              host:port:ip is curl-style DNS override with TLS
                              validation left ON. --no-confirm skips the
                              provider check; exit 3 = a real mismatch.
                              The PIR server never learns the address, and it
                              never enters a CSV, log line, or error message —
                              only `found` / `provider_match` /
                              `provider_hex_match`, one bit each. The confirm
                              call is the one exception: it asks the INDEPENDENT
                              provider about the same address in plaintext,
                              which is the check itself (--no-confirm skips it).
                              Budget: t_total = build + head + sync + answer +
                              setup + finish + residual, residual defined as the
                              subtraction. Blocks CSV covers every delta fetch,
                              follow-loop and in-trial alike.
time-setup --state <file> [--out <json-path>]
                              C13: loads the state file exactly like mainnet
                              does, times a full PIR setup recompute over it
                              (setup_seconds), and separately proves
                              persisted_hints_exact_match — the persisted,
                              incrementally patched hints reproduced byte-for-
                              byte from the persisted seed and the store's own
                              cells, exiting non-zero if they differ (the two
                              sampled decode checks are diagnostics only, never
                              gating the exit code). Run with the server
                              stopped, on the campaign binary, before a
                              measurement window opens.
```

The browser front end (`web/`, `crates/risepir-wasm`) runs the *same* rewind
client compiled to wasm **in the page**, so the address never leaves the
browser. Same origin as the PIR transport on purpose (no CORS, no mixed
content, `connect-src 'self'` CSP). Assets are read once at startup — restart
after editing `web/*`. First load is the whole product constraint: 46.51 MB at
`--partial-capacity 1000000`, but **553.82 MB at the real complete mainnet set**
(**204,714,034** accounts as of 2026-09-03 — 200,503,969 when this hint size
was first measured, 201,059,658 at the 2026-07-31 round, 203,879,841 after
the 2026-08-19 re-bootstrap; "The live GCP deployment" below has the full
lineage — at the unchanged `(arity 2, bucket_size 4)` geometry of ADR-0034):
**measured on the wire 2026-07-27** and re-confirmed live 2026-09-03,
`/setup` = 553,819,345 B = that hint plus 145 B of framing; was **830.73 MB**
at the `(arity 3, bucket_size 4)` lineage this box ran until then; the "588
MB" once quoted here predates both, computed against an assumed ~130 M). A
complete-set client now holds **1.11 GB** resident once `A` is expanded (was
1.66 GB) — a computed estimate for the browser client (ADR-0034); the CLI
`client` measured **1,156,829,184 B (1.16 GB)** resident on 2026-09-03
(`docs/deployment-numbers.md` C12). That is where the CLI `client` takes
over. Its residual trust — you trust whoever serves the page — is stated on
the page itself, not just in the ADR.

Feed = dRPC keyless (traces); reconcile = publicnode keyless (independent
operator). `"latest"` = **finalized**, ~13 min behind the public head, by design
(ADR-0007) — conformance checks must compare at an explicit height, never at
"latest"-vs-"latest".

## The live GCP deployment

Project **`<your-project-id>`**, VM **`risepir-c3d`** (**`c3d-highmem-16`: AMD
EPYC 9B14 (Zen 4), 8 cores / 16 vCPU, 128 GB, 250 GB pd-balanced disk**,
Debian 12, `us-east4-a`); repo at `~/build-4` (a fresh clone at the campaign
commit), server runs in tmux session `risepir` with `--state
~/risepir-state.bin`, logs at `~/server-complete.log`. The Mac's `gcloud` +
`gh` are authenticated; the VM is drivable non-interactively.

**Migrated 2026-09-02** from VM `risepir` (`e2-highmem-8`, 8 vCPU / 64 GB,
250 GB disk, `us-central1-a`) — cross-region, because `c3d-highmem-16`/`-8`
were stocked out in every `us-central1` zone that day. The old VM is
`TERMINATED`, kept only as a snapshot backup until this deployment is
verified end to end; deploy.md §5.11 has the full record.

Since **2026-07-26 it serves the COMPLETE mainnet set** — `GET /mode` = 1, not
the partial demo. That is what the 64 GB machine is for. On **2026-07-27 it was
re-bootstrapped onto ADR-0034's `(arity 2, bucket_size 4)`** (deploy.md §5.4),
and on **2026-07-31 again onto the `xxh3_128`/`RPST3` lineage** after the
`0f3b99b` pin bump (deploy.md §5.8), from a fresh snapshot: **201,059,658**
nonzero accounts (was 200,503,969), server DB **23.62 GB**, load 0.749, state
file **24,176,139,523 B (24.18 GB)**. The geometry has not moved across either
of the last two rounds — same 67,108,864 buckets, `plaintext_bits` 8,
cells/slot 22 — so every size in `docs/numbers.md` §4 is unchanged
(re-confirmed live 2026-09-03, campaign start, block 25,892,719).

That **201,059,658** figure was the 2026-07-31 round's count; the box was
re-bootstrapped once more on 2026-08-19, a round this repo's docs never
recorded, and loading the resulting state file on the new `risepir-c3d` host
reported **203,879,841** accounts in 113.4 s (deploy.md §5.11). The count has
moved again with the chain since: **204,714,034** accounts, verified live at
the 2026-09-03 campaign start (block 25,892,719) — the figure every current
claim elsewhere in this file now cites, at the same geometry (load now
0.7626, was 0.7490 at 201,059,658). `C11` (the campaign's own account count)
is measured independently by the campaign binary and is expected to track
this.

**(2026-07-31 round, `e2-highmem-8`; superseded for catch-up by the
`--prefetch` note above — see deploy.md §5.11 for the 2026-09-02 replay
rates.)** Measured 2026-07-31, start to caught-up: **~1 h 55 min** — 451 s
snapshot ingest, 12 min 46 s to the first saved state file, then 10,816
blocks of replay at **1.72 blocks/s** (not the ~1 s/block the runbook long
assumed). It costs
**~$23.5/day running** on the `c3d-highmem-16` (was **~$8.60/day** on the
`e2-highmem-8`; both verified against the Cloud Billing catalog, deploy.md
§5.11), so stop it when idle.

A large catch-up (this migration's own was ~52,000 blocks) is faster with
**`--prefetch <k>`** (ADR-0047, deploy.md §5.11): depth 4 sustained
**3.7–4.1 blocks/s** against the plain loop's ~1.0–1.2 blocks/s
(dRPC-bound); depth 8 fetched no faster and pushed more load onto the
keyless fallback providers while the reconcile backstop was already dark
from the lag, so 4 is the depth to reach for first on a deep catch-up.

The complete-set per-block patch time is now measured directly by the
2026-09-03 measurement campaign on `risepir-c3d` (`c3d-highmem-16`): applying
one block (store mutation + fold + hint patch), n=959, **mean 5.573 ms, p50
4.783 ms, p95 10.975 ms**, at mean K ≈ 433 mutations/block (was ~11.1 ms at
K≈310 on the e2-highmem-8, 2026-07-31). `docs/deployment-numbers.md` carries
the full table.

It is **public** at <https://demo.risepir.org> (Caddy + Let's Encrypt in front
of a loopback-only `:8645`; deploy.md §3.7), with the old
`private-eth-getbalance.duckdns.org` still served alongside it during the
overlap. Only 80/443 are open — `:8545` and `:8645` are never reachable from
outside (re-verified 2026-08-17, §5.9, with `:443` as a positive control —
a timeout on the private ports means nothing unless the public one answered
from the same machine at the same time).

**The URL to cite is <https://risepir.org>, not `demo.`** Since 2026-08-17
(ADR-0043) the apex is an always-on static page on Cloudflare Pages — *not*
this VM — so a cited link resolves on the many days the VM is stopped. It
carries the numbers, screenshots of a real lookup, and ADR-0019's
residual-trust disclosure, and links onward to the demo. `demo.` is the
intermittently-available origin and fails hard when the VM is off, which is
exactly why the apex exists.

Since **2026-08-17** the VM has held a reserved static IP, so the address
does not move across stop/start and there is **no DNS step in the ordinary
start path** — the old "run `duckdns-update.sh` *first*, the IP just
changed" rule is gone. **That reservation is now `risepir-ip-east4` =
`35.199.37.209`** on `risepir-c3d` in `us-east4-a` (was `risepir-ip` =
`136.115.93.177` on `risepir` in `us-central1-a`; the old reservation was
detached when that VM was retired — deploy.md §5.11). The `demo.` record is
deliberately **DNS-only (unproxied)** at Cloudflare: proxying would
terminate TLS at a third party that could then serve a modified wasm client,
which is exactly the code-delivery trust ADR-0019 discloses (threat model
§4.2). Never turn the orange cloud on for it.

**The DNS flip across this migration is a manual step, still pending.**
`demo.risepir.org` currently resolves to the *old*, now-detached address —
Cloudflare DNS is a dashboard-only operation with no API path in this repo
(deploy.md §3.7), so nobody has updated it yet. To cut over: point the `A`
record at `35.199.37.209`, unproxied (grey cloud, as above); the Caddy
certificate on the cloned disk is already valid for `demo.risepir.org`
(until ~2026-11-15), so no reissuance is needed, but Caddy's in-memory
certificate cache does not notice a plain config reload (deploy.md
§3.7/§5.9), so the safe move after the flip is **`systemctl restart
caddy`**, never `reload`.

**Two traps specific to a fresh clone or a migrated disk** (both hit during
the 2026-09-02 migration, deploy.md §5.11): a cloned disk's `target/` is
compiled for the *old* CPU and `cargo build` cannot tell `target-cpu=native`
changed, so it silently reuses stale kernels — always `cargo clean` (or
clone fresh) before the first build on a new machine. And `web/client.wasm`
is a build artifact, absent from a fresh clone — `--web web` fails at
startup (`web/client.wasm: No such file`) until `cargo run -p xtask
--release -- web` runs once.

```bash
gcloud --quiet compute ssh risepir-c3d --zone us-east4-a --command='...'
# resume after a stop — the IP is static, so no DNS refresh:
gcloud compute instances start risepir-c3d --zone us-east4-a
# normal restart: the 24.18 GB state file is loaded, then missed blocks replay
gcloud --quiet compute ssh risepir-c3d --zone us-east4-a --command='tmux new-session -d -s risepir \
  "cd ~/build-4 && exec ./target/release/risepir-rpc mainnet \
   --state ~/risepir-state.bin --web web >> ~/server-complete.log 2>&1"'
```

While following, the server rewrites the state file every `--save-interval`
seconds — default **21600** (6 h) now that `--journal-restore` defaults **on**
(ADR-0037: the journal, not the full save, is what bounds replay after an
ungraceful kill), or **1800** (30 min, ADR-0025's original value) if
`--no-journal-restore` is passed; an explicit `--save-interval` always wins
over either default. A crash or missed Ctrl-C then replays at most the
journal's tail (well under a second per block) rather than the whole
interval. Every save logs a `state saved: block …, … GB in …s` completion
line.

Every runtime log line is prefixed with an RFC 3339 UTC timestamp
(`2026-07-30T04:12:33Z risepir-rpc mainnet: …`, `logln!` in
`crates/risepir-rpc/src/logging.rs`); the message after it is unchanged, so
existing greps match — but a **`^`-anchored** one must drop its anchor. CLI
banner/usage/argument output stays untimestamped on stdout, deliberately.
The log does **not** rotate on its own in the tmux shape (it hit 66.79 MB
before this was addressed): install `ops/logrotate/risepir`, whose
`copytruncate` is load-bearing — the server never reopens its log, so a
rename-then-create rotation would leave it writing to the renamed inode
while the live file stays 0 bytes (deploy.md §4 "Log timestamps and
rotation"). Never `kill -HUP` it to force a reopen: there is no SIGHUP
handler, so that terminates the server.

Beside it sits `<state>.journal` (ADR-0026): one small per-block delta,
appended and fsynced as each block applies, rotated to a fresh file at every
save. Always written once a first save exists. Restoring from it is now the
default (`--journal-restore`, ADR-0037 — ADR-0026 shipped it opt-in behind a
soak period that has since held without a single corruption report): a
restart replays it and resumes above the last save's height instead of at
it, recovering to seconds instead of minutes at a fraction of the disk-write
cost. `--no-journal-restore` opts back out — a restart then only scans and
reports it (`journal intact: N records to block X`), the original ADR-0026
soak signal, still available to anyone who wants it.

The `exec` is load-bearing: it makes the binary *be* the tmux pane process, so
signalling it never involves a wrapper shell. `--web web` is what serves the
browser front end at all.

**A state file present means `--snapshot` is silently ignored** (`mainnet.rs`
prints a note and loads the file). That is the trap to know: leaving the old
*partial* state file in place would have brought the server back up in PARTIAL
mode while every flag on the command line said complete. Re-bootstrapping from
the snapshot means moving the state file aside first — and at the complete set
that costs **~16 min** (2026-07-27 record, `e2-highmem-8`; the campaign's
measured setup time will land in `docs/deployment-numbers.md`) at the
deployed `(2,4)` geometry (8 min ingest + ~6 min PIR setup (the campaign
measured 29.18 s for the setup step alone on the c3d-highmem-16,
docs/deployment-numbers.md C13) + 2 min save; it
was ~33 min at `(3,4)`), *plus* the snapshot→head
replay, which is the part that actually hurts: ~1 s/block, so a day-old
snapshot is another ~3 h. Prefer the state file.
`~/bootstrap-complete.sh` on the VM re-runs the full bootstrap.

**A second, sharper trap since ADR-0034: a geometry change turns "restart"
into "re-bootstrap."** The deployed geometry moved from `(arity 3, bucket_size
4)` to `(arity 2, bucket_size 4)`, and the state-file loader now checks the
stored geometry's arity **by name**, immediately after the header decodes and
before the (multi-GB) cells section is even read (`STORE_ARITY` in
`crates/risepir-rpc/src/state.rs`, ADR-0034 §6). A state file written by the
old 3-ary binary is therefore *refused*, not silently loaded and not
misreported as `Corrupt` — the error names the cause (a previous geometry
lineage) and the fix (move `--state` aside, re-bootstrap). This fired for real
on 2026-07-27, exactly as designed — `exit 1` with that message, before any of
the 36 GB was read — and the box was re-bootstrapped past it (deploy.md §5.4).
The superseded 3-ary file was kept on the box as a rollback until **2026-07-29,
when it was deleted** to reclaim its 33.77 GB (deploy.md §5.4): no binary built
from this tree can load it — that is precisely what `STORE_ARITY` guarantees —
so the "rollback" it offered was never a restart, it was a code revert to the
3-ary lineage *plus* hours of replay onto a by-then-stale file. A fresh
bootstrap is the honest path if `(2,4)` ever needs reverting. Both lineages
now agree on `(2,4)`, so a plain restart works again; the trap is live for the
*next* geometry change, not for this one.

**A third trap, and the nastiest, since the `0f3b99b` pin (2026-07-31): a
change to the *hash* is invisible to every guard that checks the
*geometry*.** The pin bump moved the primitive's item hash from `xxh3_64` to
`xxh3_128`, so every key lands in a different bucket — while `arity`, the
`ValueCodec`, `bucket_size`, `fingerprint_bits`, `plaintext_bits` and
`num_buckets` all stay byte-identical. `STORE_ARITY` and
`check_geometry_lineage` cannot see it *by construction*: they compare
parameters, and the parameters are still correct. An old file would have
loaded clean and then missed on every lookup — `0x0` for accounts that exist,
across the whole set. So the **state format version** carries the hash lineage
now: `RPST2` → **`RPST3`**, and `RPST1`/`RPST2` are refused by name before a
cell is read (ADR-0042's outcome note). The general lesson: a guard that
compares parameters cannot catch a change in the *function* those parameters
configure — only the format version can.

**That migration has been RUN (2026-07-31, deploy.md §5.8).** The refusal fired
for real on the production 24.18 GB file — `exit 1` in under a second, before a
cell was read — and the box was re-bootstrapped past it from a fresh snapshot.
The VM now holds an **`RPST3`** file and a plain restart works again; the trap
is live for the *next* hash change, not for this one. The superseded
`~/risepir-state.bin.rpst2-20260731` and `~/risepir-state.bin.pre0728`
(24.18 GB each) are evidence only — no binary from this tree can load them —
and can be deleted to reclaim ~48 GB.

(The external IP is reserved and stable, so nothing has to be refreshed
after a start. An SSH tunnel works as before:
`gcloud compute ssh risepir-c3d --zone us-east4-a -- -L 8545:localhost:8545`.)

Stopping the meter — in this order:

```bash
gcloud --quiet compute ssh risepir-c3d --zone us-east4-a \
  --command='pkill -INT -f "^\./target/release/risepir-rpc" && sleep 90 && tail -1 ~/server-complete.log'
#   → wait for "state saved; exiting" — at the complete set this writes the
#     24.18 GB state file (the `(2,4)` lineage live since 2026-07-27; the
#     final save of the old 36.26 GB `(3,4)` file took ~2 min), so allow well
#     over the 20 s that sufficed in partial mode
gcloud compute instances stop risepir-c3d --zone us-east4-a
```

The **anchored** pattern matters: a broad `pkill -f risepir-rpc` also kills the
tmux wrapper shell, tmux then SIGHUPs the pane group, and the server dies
mid-save with a 0-byte `.tmp` (this happened on 2026-07-19; harmless in partial
mode, but **now genuinely expensive** — a complete-set re-bootstrap is ~33 min
of CPU plus the catch-up replay). When checking from `gcloud … --command`,
bracket the pattern (`pgrep -f "risepir-rp[c]"`) or the probe matches its own
ssh wrapper. VM SSH key and the GitHub account key `risepir-gcp-vm` are already
set up; at `c3d-highmem-16` the VM burns **~$23.5/day** while running (was
**~$8.60/day** at `e2-highmem-8`, itself up from ~$0.80/day as an
`e2-medium`; both current figures verified against the Cloud Billing catalog,
deploy.md §5.11), ~$25/mo stopped (250 GB pd-balanced disk only — was quoted
here as ~$10/mo on the old host).
