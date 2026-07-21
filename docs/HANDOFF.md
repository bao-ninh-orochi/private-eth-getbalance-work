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

**1. The complete-set run (Stage 1.d) — gated on the user's BigQuery step.**
`docs/deploy.md` §2.1 has the exact gate query (freshness + nonzero count +
snapshot block in one shot) and export commands. When the shards exist:
`risepir-rpc mainnet --snapshot … --snapshot-block … --snapshot-accounts …
--state …` on a 16–24 GB box (deploy.md §2.3). Watch the geometry line it prints
before allocating. Expect the catch-up replay to take hours for a day-old
snapshot (~1–2 s/block on keyless tiers).

**2. Known open items, in priority order:**
- **Withdrawal-recipient hard refresh** (deploy.md §4): a one-time absolute
  re-read of the ~32 k withdrawal recipient addresses vs an archive RPC, to
  clear any credit ambiguity at the snapshot join. Small utility; not built.
- The catch-up replay is serial (~1–2 s/block); Xatu balance-diff chunks
  (`docs/data-acquisition.md` path 3) would bulk-replay the snapshot→head gap if
  the join gets long.
- Upstream PR candidates (offer, don't block on): batch-mutation API; seed
  injection in `server_setup` (`docs/verification.md`).
- Optional polish: MetaMask walkthrough screenshots; a public deployment on the
  Oracle free-tier box.
- **Web front end, deliberately deferred** (ADR-0019 records why, and what each
  needs): caching the hint across visits (sound only against the delta-ring
  retention check — a `409` must force a re-download); public exposure, which
  needs a hostname + certificate before it is honest to serve client code over
  the open internet; and serving the page from a *different* party than the PIR
  server, which is the stronger arrangement for the code-delivery trust.

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
  `commit.gpgsign=true`). Push directly to `main` — the user's private repo, no
  fork, no PR, no AI attribution trailers.
