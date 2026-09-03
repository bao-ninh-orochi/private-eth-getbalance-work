# Handoff prompt — continue building private-eth-getbalance

*Paste this into a fresh session to continue. It is self-contained; the linked docs are
the detail.*

---

You are continuing a proof-of-concept **private `eth_getBalance` over RisePIR** (LWE
keyword-PIR). Work in your checkout of this repository (git repo, remote
`orochi-network/private-eth-getbalance`, branch `main`).

**Before writing code, read, in order:** `docs/plan.md` (authoritative spec),
`docs/deploy.md` (the runbook + recorded live evidence), `docs/adr/README.md`
(decisions; ADR-0017/0018 are the newest), `docs/sync.md`. The PIR primitive is a
**pinned git dep** (`bao-ninh-orochi/IKPIR` @ `v0.2.0-perf`); a local checkout of the
primitive may live alongside this one — **read its source for exact signatures;
never guess an API** (but do not build against the checkout: it moves; the pin is the
truth).

## Where things stand (2026-07-21)

**The PoC works against real mainnet.** `risepir-rpc mainnet --partial` follows
finalized blocks over keyless public RPC (dRPC traces ⊕ withdrawals), serves private
`eth_getBalance` on `:8545` at real LWE parameters, reconciles sampled accounts
against an independent provider (publicnode), and persists/reloads full PIR state.
Recorded evidence (deploy.md §5): 8/8 private queries byte-exact vs publicnode on
live-changed accounts; in-loop reconcile exact at every checkpoint; strict
not-found errors (never `0x0`) in partial mode; Ctrl-C state save + instant reload.

**There is now a browser front end** (ADR-0019): `crates/risepir-wasm` compiles the
*same* `risepir-client` rewind client to wasm32, and `risepir-rpc mock|mainnet --web
web` serves `web/` from the PIR port's own origin, so a visitor's address never
leaves the page. Live-verified 2026-07-21 (`docs/deploy.md` §5.1): a real mainnet
balance fetched in a browser, byte-exact against `rpc.flashbots.net` at the same
height. First load is 49 MB of hint at `--partial-capacity 1000000`; client compute
is ~10 ms/lookup. Its residual trust (you trust whoever serves the page; the network
still sees who is asking) is stated on the page itself.

513 tests green (as of 2026-09-03); `xtask conformance` PASS (1201×120, 15117 checks, 0 mismatches); the
live feed gate (`cargo test -p risepir-feed --release -- --ignored`) validates the
diffMode trace parsing byte-exactly against a second provider; and two browser gates
(`node web/test/e2e.mjs` / `node web/test/browser.mjs`, both against a running
`--web` server, neither needing npm).

Structural work this round: IKPIR deps pinned (the old path deps were dead);
**verified fp ∧ `key_tag` store ops** close the fingerprint-collision
wrong-answer class (ADR-0017 — also answers the store-all-vs-nonzero-only
question: nonzero-only stays, hazard closed properly); withdrawal credits ride
`BlockUpdate::credits`, resolved server-side against the verified prior
(ADR-0018); snapshot ingest (`risepir_feed::snapshot`); mainnet rpc feed
(`risepir_feed::rpc`); state files (`risepir_rpc::state`, RPST1); partial mode.

## What is left

**1. ~~The complete-set run (Stage 1.d)~~ — DONE 2026-07-26.** The gate query,
the export, and the complete-set run all happened; the live GCP box now serves
the complete set. See `docs/deploy.md` §2.1 (recorded gate output), §2.3
(revised sizing) and §5.3 (live evidence).

The one number that mattered: mainnet had **200,503,969** nonzero accounts at
that first measurement (2026-07-26) — not the ~100–130 M this file and the
runbook had assumed — and the live count has grown since, to **204,714,034**
as of 2026-09-03 (`CLAUDE.md`'s "The live GCP deployment" has the full
lineage). At the geometry deployed then (`arity 3, bucket_size 4`), that made
the server DB **35.43 GB** rather than 10–13 GB. The "16–24 GB box" advice
was wrong by more than 2× regardless;
the deployment ran on a 64 GB `e2-highmem-8` at the time (migrated
2026-09-02 to a 128 GB `c3d-highmem-16`, briefly in `us-east4-a` for a
measurement campaign and back in `us-central1-a` since 2026-09-03 —
deploy.md §5.11/§5.12).
ADR-0034 has since retuned the
deployed geometry to `(arity 2, bucket_size 4)` at a higher target load, and the
live box **was re-bootstrapped onto it on 2026-07-27**: 23.62 GB server DB, a
24.18 GB state file, 16 min end to end (`docs/deploy.md` §5.4). Anything that
still quotes the 35.43 GB / `arity=3` / 830.73 MB figures without saying they are
the superseded `(3,4)` lineage is stale.

**2. Known open items, in priority order:**
- **~~Post-bootstrap snapshot audit~~ — DONE (ADR-0040), and re-scoped: the
  suspect population is not what this file used to say.** The original
  version of this item claimed the suspect population was "exactly the
  accounts touched in the final ~N blocks before `--snapshot-block`" and that
  "there are few of them" — **both are now known to be wrong.** Re-measuring
  the **export** at scale (deploy.md §2.1, ADR-0040) found the error decays
  with distance from the boundary but does **not** vanish: 6.9% of accounts
  touched in the 2000 blocks before the boundary were wrong, still 5.47% at
  depth (1000,2000], and — independent of any recency window at all — a
  population-wide random sample measured **0.33%** wrong (Wilson 95% CI
  [0.09%, 1.21%], implying ~668,000 of the 200,503,969 accounts). "Few, and
  bounded to a recent window" was never true; "a measured, disclosed
  residual across the whole set" is the honest description of the *export*.

  **The deployment itself is not the export**, and both were measured: the
  same population check run directly against the live server found 0 wrong
  of 200 (Wilson 95% CI [0.00%, 1.88%]) — the ordinary forward replay heals
  most of what the export got wrong, for free — but re-checking the
  *specific* rows already flagged as wrong found 28/150 window-wrong and
  22/100 funded-but-absent accounts still wrong days later. Root cause,
  verified with `bq show` (not inferred): the source is a table rebuilt once
  daily with no block-number column, so the gate query's "last block of the
  previous UTC day" is an assumption about that rebuild's instant that **can
  fail in either direction** — every disagreement is either the export
  reflecting a state *after* the declared block (heals unconditionally via
  ordinary replay) or *before* it (never heals via forward replay alone,
  which is exactly what `--snapshot-rewind` reaches backward for). See
  ADR-0040 for the full measurement, both populations' numbers, and the
  causal model; citing only the export's 0.33% or only the deployment's
  0/200 each mislead in a different direction.

  Three mechanisms now exist, none of which alone closes the gap:
  **`--snapshot-rewind`** (default 2000, on by default) targets exactly the
  "export reflects a state before B" half by re-deriving a window from the
  chain's own absolute post-state during the ordinary replay — it does
  nothing for, and needs to do nothing for, the "after B" half, which heals
  on its own; **the post-bootstrap audit** (`--snapshot-audit-samples`,
  default 512) measures and discloses the population-wide baseline on every
  bootstrap (console line + `<state>.audit` sidecar + one `GET /healthz`
  line, reporting loudly above a 1% Wilson-lower-bound threshold but never
  refusing to serve) — its uniform sampling tracks that baseline but is not
  built to catch the boundary-concentrated residual itself; **`--hard-refresh
  <file>`** is the general-purpose quorum-verified correction tool for a
  *known* suspect list (idempotent, runs in the background, never blocks
  serving or following), which is what actually reaches the concentrated
  residual once one is identified. None of the three is automatic
  population-wide correction — deciding which addresses warrant a
  `--hard-refresh` run past what the audit samples is still an operator
  judgment call. See ADR-0040 for the full measurement and
  `docs/deploy.md` §2.1/§2.2 for the procedure.
- **~~`--hard-refresh` has no rate-limit backoff~~ — the backoff SHIPPED
  (ADR-0041, PR #39, 2026-07-29). What is still open is the correction *run*
  itself.** The original diagnosis held exactly: a 57,646-address pass logged
  **67,791 fetch failures, all HTTP 429**, and **zero** `providers disagree`
  warnings — every one of the 50,813 skips was a fetch that never landed, so
  the binding constraint was rate limiting, not disagreement. Each fetch is
  now retried 4× with per-address-jittered exponential backoff
  (`MAX_FETCH_ATTEMPTS`, `backoff_delay`); `CONCURRENT_ADDRESS_CHECKS` stayed
  at 8 deliberately, since fan-out is the wrong knob for a limit nobody
  knows. Measured (deploy.md §5.7): skip rate **88.1% → 8.0%**.

  **But that "after" was a 1,000-address sample, not a full pass** — so the
  57,646-address correction run has still never completed, and ADR-0040's
  population-wide finding (the export is wrong for ~0.33% of *all* accounts,
  ~668k, not just near the snapshot boundary) is still only partly
  corrected. `--hard-refresh` fails safe and is idempotent, so the remaining
  work is simply to run it to completion against the live box and record the
  result — which needs the deployment up, and after the `xxh3_128` pin bump
  that means a re-bootstrap first (deploy.md §4).
- **Withdrawal-recipient hard refresh** (deploy.md §4): a one-time absolute
  re-read of the ~32 k withdrawal recipient addresses vs an archive RPC, to
  clear any credit ambiguity at the snapshot join. Partly superseded:
  `--hard-refresh` *is* that utility now, and the 2026-07-28 run included
  12,004 withdrawal recipients from the 20,000-block window — but the full
  ~32 k set has not been swept.
- The catch-up replay is serial (~1–2 s/block); Xatu balance-diff chunks
  (`docs/data-acquisition.md` path 3) would bulk-replay the snapshot→head gap if
  the join gets long.
- Upstream PR candidates (offer, don't block on): batch-mutation API; seed
  injection in `server_setup` (`docs/verification.md`); widening
  `segmented_cuckoo::SUPPORTED_BUCKET_SIZES` past 4 with a measured
  `MAX_LOAD_FACTOR` for it — `(2,7)` would reach load 0.8536 at 20.67 GB, and
  ADR-0034 now ranks this the highest-value upstream ask for this project;
  non-power-of-two `segmented-cuckoo` segment sizes is next — that masking-based
  hash is why `num_buckets` quantizes by factors of two, and so why
  `slots` lands on the coarse `{2^t, 3·2^t, 9·2^t}` rung menu that ADR-0034 §1
  had to pick from rather than sizing freely. `xtask geometry [--fill-check]`
  is the tool for either question if it comes up again.
- Optional polish: MetaMask walkthrough screenshots. (~~A public deployment on
  the Oracle free-tier box~~ — public deployment shipped in PR #5 on GCP; the
  Oracle 24 GB free tier still can't hold the complete set, though ADR-0034
  changed the margin completely: at the live `(3,4)` geometry it was never
  close (35.43 GB DB alone), and even at the deployed `(2,4)` geometry's
  ~24.7 GB working set it is still ~0.7 GB over — a real no, just no longer a
  1.6× one. `docs/deploy.md` §2.3/§3.6.)
- ~~Hint caching is now the front end's binding constraint, not a nicety~~ —
  **built 2026-07-28 (ADR-0038).** The browser client now persists the raw
  `/setup` bytes in IndexedDB, keyed by the hint-lineage epoch, and
  `GET /setup` gained server-side `Range`/`If-Range` support
  (`crates/risepir-http/src/node.rs`) so a reload, a return visit, or a
  connection dropped mid-transfer resumes or reads from cache instead of
  paying for the whole thing again. Proven end to end against `mock`
  (`web/test/browser.mjs`: a second navigation issues **zero** further
  body-bearing `GET /setup` requests and still answers correctly; measured
  1022 ms wall clock for that second boot). What this does **not** fix: the
  *first-ever* visit to a given deployment still pays the full transfer —
  **553.82 MB** at the deployed `(arity 2, bucket_size 4)` geometry
  (ADR-0034; was **830.73 MB** at `(arity 3, bucket_size 4)`,
  `docs/numbers.md` §4c) versus 46.51 MB at `--partial-capacity 1000000` —
  and a client's resident memory once the hint decodes and `A` expands is
  unchanged at **1.11 GB** (was 1.66 GB), since that cost comes from what
  wasm holds once decoded, not from the network — ADR-0032's capacity
  pre-flight therefore still runs unconditionally, cache hit or not. ADR-0019
  originally listed caching as deferred-with-conditions (sound only against
  the delta-ring retention check — a `409` must force a re-download);
  ADR-0033 supplied the sharper lineage-epoch revalidation ADR-0038 is
  actually built against.
- **~~The IKPIR pin is one lineage behind~~ — DONE 2026-07-31, pin now
  `0f3b99b`** *(ADR-0042)*. Upstream corrected RisePIR's Lemma 2, widened
  `fingerprint_bits` to u64, and replaced the `plaintext_bits` selectors with
  δ_cell-targeted ones. ADR-0042 re-derived this repo's operating point
  against that rule and concluded **nothing moves**: κ ≈ 61 (filter-bound,
  index term 278 bits slacker), 21 bits past the κ = 40 the lemma targets.
  Crossing the pin **confirmed that prediction against the real selector** —
  `docs/numbers.md`'s §4a geometry rows came back byte-identical, so the
  deployed `(pb 8, cells/slot 22, 23.62 GB, 553.82 MB, 1.11 GB)` operating
  point is untouched and no re-bootstrap is owed for *geometry* reasons.

  **But the bump does invalidate the live state file, for a different
  reason.** The item hash moved `xxh3_64` → `xxh3_128`, so every key now
  lands in a different bucket while the entire header stays byte-identical —
  arity, codec, `bucket_size`, `fingerprint_bits`, `plaintext_bits`,
  `num_buckets` all match. Neither `STORE_ARITY` nor ADR-0042's
  `check_geometry_lineage` can see it, and an old file would load clean and
  then miss on every lookup, answering `0x0` for accounts that exist. The
  state format version now carries that lineage: **`RPST2` → `RPST3`**, and
  `RPST1`/`RPST2` are refused by name before a cell is read. **The VM's
  24.18 GB state file is an `RPST2` file and must be re-bootstrapped, not
  restarted** — see `docs/deploy.md` for the migration.

  What the adaptation actually cost, for the next time: `rev` in both
  `Cargo.toml` (3 deps) and `fuzz/Cargo.toml` (2 deps); **20**
  `*_max_plaintext_bits` call sites across 9 files, of which only **two**
  were production (both in `Geometry::for_num_buckets`); the client's
  `ct_eq_u32_mask` collapsed into `ct_eq_u64_mask` now that fingerprints are
  `u64`; two birthday-search `HashMap` keys widened; and three **Frodo**
  `plaintext_bits` pins moved (11→10, 8→7) because Eq. 8's weak tail was
  replaced by an explicit Bernstein tail. **Every SimplePIR pin held** —
  which is the backend this repo deploys (ADR-0002), and why the geometry
  survived.
- **Web front end, remaining deferrals** (ADR-0019 records why, and what each
  needs): ~~public exposure~~ (shipped, PR #5); and serving the page from a
  *different* party than the PIR server, which is the stronger arrangement for
  the code-delivery trust.

## The binding rules (do not violate)

- **Never return a wrong answer.** Erroring is fine; labelled-stale is fine; a
  silently wrong balance is total failure. Every `NotFound`/`DecodeFailed`/
  `FingerprintAmbiguity`/`CorruptStoredValue`/strict-partial path exists for
  this. When in doubt, fail loudly.
- **Never hardcode `plaintext_bits` or geometry** — derive via
  `risepir_proto::Geometry`.
- **Validate every length before allocating** — the server ingests
  attacker-controlled blobs; malformed input must give a clean error, never a
  panic or OOM.
- **Partial mode never answers `0x0` for an untracked account**, and never
  applies a credit to one (mainnet.rs's filter) — absence only means zero for a
  complete set (ADR-0015/0017).
- **Run the tests you write and report real output.** The live gates exist —
  use them (`-- --ignored`, and a `--partial` smoke run) after touching the
  feed or the apply path.
- Commits: signing works (`ssh-add --apple-use-keychain` was run;
  `commit.gpgsign=true`). **Fork-based, reviewed and merged by someone else —
  the flip from an earlier revision of this file, which described a
  self-merge, no-fork process.** `origin` is a personal fork
  (`bao-ninh-orochi/private-eth-getbalance-work`); `upstream` is
  `orochi-network/private-eth-getbalance`. Open PRs from the fork branch
  against `upstream`'s default branch, as a draft while work is in progress
  and ready only once implementation-complete **and** CI green; a separate
  reviewer agent (`on-unknown-fish`) reviews and merges — the author never
  self-merges. No AI attribution trailers or footers.
