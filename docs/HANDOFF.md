# Handoff prompt — continue building private-ETH-getBalance

*Paste this into a fresh session to continue. It is self-contained; the linked docs are
the detail.*

---

You are continuing a proof-of-concept **private `eth_getBalance` over RisePIR** (LWE
keyword-PIR). Work in `/Users/admin/Documents/private-ETH-getBalance` (git repo, remote
`bao-ninh-orochi/private-ETH-getBalance`, branch `main`).

**Before writing code, read, in order:** `docs/plan.md` (authoritative spec),
`docs/deploy.md` (the runbook + recorded live evidence), `docs/adr/README.md`
(decisions; ADR-0017/0018 are the newest), `docs/sync.md`. The PIR primitive is a
**pinned git dep** (`bao-ninh-orochi/IKPIR` @ `3d60fa7`); a local checkout lives at
`/Users/admin/Documents/CANS2026/RisePIR` — **read its source for exact signatures;
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

186 tests green; `xtask conformance` PASS (1201×120, 15117 checks, 0 mismatches); the
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

The one number that mattered: mainnet has **200,503,969** nonzero accounts, not
the ~100–130 M this file and the runbook had assumed. At the geometry deployed
then (`arity 3, bucket_size 4`), that made the server DB **35.43 GB** rather
than 10–13 GB. The "16–24 GB box" advice was wrong by more than 2× regardless;
the deployment runs on a 64 GB `e2-highmem-8`. ADR-0034 has since retuned the
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
- **Withdrawal-recipient hard refresh** (deploy.md §4): a one-time absolute
  re-read of the ~32 k withdrawal recipient addresses vs an archive RPC, to
  clear any credit ambiguity at the snapshot join. Small utility; not built.
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
  `commit.gpgsign=true`). **Branch → PR → self-merge** into `main` (the `PGR-###`
  rules): no fork (`origin` *is* this repo, there is no `upstream`), CI green
  before merging, then `gh pr merge <#> --squash --delete-branch`. `main` is
  protected by convention — do **not** push to it directly, which is what an
  earlier revision of this file said. No AI attribution trailers or footers.
