# Roadmap — from working PoC to something that could run for real

*v2, updated 2026-07-21 after hardening rounds 1–2 (PR #1 + follow-up).
This is the complete, prioritized inventory of everything between this PoC
and a production-grade deployment: the original 2026-07-21 survey, updated
with what landed, plus the independent audit findings that survey did not
list. Items name the evidence that produced them; approaches named here are
priors, not orders — better paths get taken and recorded (new ADRs in
`docs/adr/README.md`; ADR-0021 is the newest).*

---

## 0. Where the repo actually is

| | state |
|---|---|
| Tests | 190+ across 8 crates; `xtask conformance` (byte-exact, network-free); live feed gate; web e2e + headless-browser gates |
| CI | `.github/workflows/` — clippy `-D warnings`, tests, rustdoc `-D warnings`, wasm32 check on every push; conformance on PRs; live gate + fuzz nightly; `cargo-deny` |
| Fuzzing | 5 coverage-guided targets: every wire decoder, `BlockDelta`, the state-file loader |
| License | dual MIT/Apache-2.0, inherited by every crate |
| Threat model | `docs/threat-model.md` (ADR-0020) — per-adversary guarantees and stated non-guarantees |
| Timeouts / limits | both listeners: body caps + request timeouts; `/setup` concurrency cap; client side: per-endpoint response caps + connect/stall timeouts |
| State file | `RPST2`: whole-file xxh3, allocation bounded by file size, legacy `RPST1` readable; backup/restore drill in deploy.md |
| Shutdown | SIGINT **and** SIGTERM → graceful state save; listener crash → loud process exit (never a silent half-outage) |
| Deployment | GCP VM; systemd unit (`ops/systemd/risepir.service`) with the graceful-stop semantics encoded; `GET /healthz` |
| Toolchain | pinned 1.96.0; MSRV declared; rustfmt.toml present (gate deferred, §2 item 10) |
| TLS | **live** — Caddy + Let's Encrypt in front of a loopback-only `:8645` (`ops/caddy/Caddyfile`, deploy.md §3.7); public origin verified 13/13 + 20/20 |
| Rate limiting | **none** beyond the `/setup` cap |
| Observability | `GET /metrics` (Prometheus text) + `GET /status` (a polling page) ship block lag, answer latency, error rate by class, store occupancy, save/journal outcomes (ADR-0039, §2 item 2); logging is still `println!`/`eprintln!` only — no `tracing` |
| Panic audit | **not done** — `clippy::unwrap_used`/`expect_used` not yet enabled (§2 item 1) |

~~**One key still missing:** CI needs the `IKPIR_TOKEN` secret (fine-grained
PAT, IKPIR repo only, Contents read-only — deploy keys are *disabled* on
that repo). Until the user mints it, every CI job fails early with an
explicit `::error`.~~ **Closed** — `bao-ninh-orochi/IKPIR` is public now,
pinned at the `v0.2.0-perf` tag (was `v0.1.0-perf`; ADR-0046); CI needs no
secret, and the `IKPIR_TOKEN`/`insteadOf` plumbing is gone (ADR-0045).

## 1. Done — with the finding that motivated each

**Round 1 (PR #1):** A0 threat model (ADR-0020); B1 CI + A4 supply chain
(ADR-0021); B2 wire-decoder fuzzing; B5 toolchain pin; C1 systemd unit;
C3 timeouts + `/setup` cap; C7 `/healthz`; E1 dual license; E2
SECURITY/CONTRIBUTING; E4 README; clippy-clean workspace.

**Round 2 (independent audit of every input path and process boundary):**

- **State file could silently serve a flipped bit.** v1's structural checks
  missed in-cells corruption; a flipped fingerprint bit reads a colliding
  account as `0x0` — the forbidden failure, delivered by a disk. → `RPST2`
  whole-file xxh3 (legacy readable), plus `setup_len` bounded by the file's
  own size before allocation (an OOM lever, closed), plus a fuzz target on
  the loader.
- **A hostile server could OOM or hang the client.** `PirHttpClient`
  buffered response bodies unboundedly with no timeouts — the §4.2
  adversary controls those bytes. → per-endpoint body caps checked
  streaming, connect + read-stall timeouts.
- **A crashed listener left a half-alive process.** Five
  `tokio::spawn(axum::serve(..).expect(..))` sites died silently while
  block-following continued. → listener crash now exits the process loudly;
  the supervisor restarts the unit.
- **Bare SIGTERM skipped the state save** (only Ctrl-C was handled; systemd
  and docker send SIGTERM by default). → both signals now take the graceful
  path; the unit's `KillSignal=SIGINT` is defense in depth, not a
  requirement.
- **`:8545` had no body cap or timeout** (the PIR port did). → 1 MiB cap +
  60 s timeout.
- Cheap professionalism: MSRV declared everywhere; rustdoc `-D warnings`
  gate (was already clean); backup/restore drill + TLS recipe in deploy.md.

## 2. The remaining queue, by effort ÷ impact

1. **B3 — Panic audit** *(next; unblocked now that the web branch landed).*
   Enable `clippy::unwrap_used`/`expect_used` workspace-wide, then fix or
   `#[allow]`-with-justification each site. ~330 sites; the value is the
   reviewed enumeration, not deletions. Prioritize anything reachable from
   network input; test modules get a blanket allow. 2–3 days.
2. ~~**C4 — Observability.**~~ **Largely done 2026-07-28** — `GET /metrics`
   (hand-rolled Prometheus text exposition, no new dependency) and
   `GET /status` (a small polling operator page, same web-asset mechanism
   as the browser front end) ship block lag (`risepir_finalized_block`
   against `risepir_head_block` — the number that would have shortened the
   2026-07-28 diagnosis from 35 minutes of SSH + hand-rolled `curl` loops to
   one glance), an answer-latency histogram (timed around
   `RisePirServer::answer` only, timing-side-channel-safe by construction —
   ADR-0039's own analysis), error rate *by class* (`ServerError`/
   `WireError`'s existing taxonomies), store occupancy, and state-save/
   journal outcomes (ADR-0039). What is *not* done: `tracing` /
   structured logging — this deployment's logs are still `println!`/
   `eprintln!` text, and federating `/metrics` into an actual Prometheus
   server (as opposed to exposing the exposition format) is left to
   whoever operates one.
3. **A3 — Feed integrity middle rung.** Full quorum needs a second keyless
   *trace* provider (publicnode serves no traces), which does not exist
   today; the achievable rung is full-coverage per-block reconciliation —
   verify **every** touched account (batched `eth_getBalance` at height)
   against the independent provider before serving that block, degrading to
   sampling under rate pressure, behind a flag + ADR. Extends
   never-wrong-answer to ingest against everything short of feed+confirm
   collusion. 2–4 days.
4. **B4 — Deterministic simulation testing.** Random block streams, random
   restarts, mid-save kills, truncated/corrupted state files, replayed and
   reordered deltas — against a `HashMap` oracle, from a reproducing seed.
   `batched_equals_per_mutation` and the RPST2 tests are the seed of the
   pattern; this generalizes it to the whole apply/persist/rewind path.
   ~1 week, the strongest remaining correctness spend.
5. ~~**C2 — TLS, deployed.**~~ **Done 2026-07-26** — Caddy + Let's Encrypt,
   listeners still loopback-only, public origin verified end to end (deploy.md
   §3.7). What it bought and what it did *not* (the DNS/CA parties now in the
   code-delivery chain) is in threat model §4.2. **Narrowed 2026-08-17**: the
   free DuckDNS hostname gave way to `demo.risepir.org`, a registered domain
   on a static IP, with DNSSEC and CAA — the "buys a domain it controls"
   mitigation §4.2 had only recommended. **Closed 2026-08-17** (ADR-0043): the
   **always-on apex page** at `risepir.org` is live on Cloudflare Pages — not
   this VM — so a cited URL survives the VM being stopped and its certificate
   lapsing. Verified serving over a valid certificate while the VM was
   `TERMINATED` (deploy.md §3.7). Cite `risepir.org`; `demo.` stays the
   intermittently-available origin. What remains on this rung is unchanged and
   sits with C3/C5: there is still no rate limiting in front of a 553.82 MB
   `/setup`.
6. **A1 rungs — signed store digests → public anchoring.** The first
   steps past "documented trust" (ADR-0020): a per-block signed digest
   makes global tampering attributable; anchoring stops split-view. Does
   **not** stop targeted lies — say so wherever described. Days each; full
   verifiable PIR (VeriSimplePIR-style) remains research scale.
7. **C5 — `/setup` behind a CDN** (immutable per epoch pin; nearly free
   once TLS/hostname exist) and real rate limiting / per-IP quotas (C3's
   second half).
8. **D1 — The complete-set run** (user-gated on the BigQuery export;
   deploy.md §2.1 has the one-shot gate query). Largest remaining *product*
   gap. With it: **D2** withdrawal-recipient hard refresh (small utility),
   **D3** Xatu bulk replay if the snapshot→head join is long.
9. **D4 — ERC-20 (`balanceOf` is a storage slot; scope to one token, e.g.
   USDC).** Highest product upside on the list. **D5** multi-query batching
   couples to the IKPIR upstream thread (the `perf/optimized` refresh
   landed 2026-07-22 at `3d60fa7`, and the f=64 / corrected-Lemma-2 refresh
   landed 2026-07-31 at `0f3b99b` — numbers re-measured each time; batching
   still needs an upstream `answer_batch` API).
10. **Hygiene tail:** ~~flip the `cargo fmt --check` CI gate on in a
    formatting-only commit~~ **DONE 2026-07-31** — the reformat touched 58
    files (+4,769/−1,217), not the ~296 estimated here, and is listed in
    `.git-blame-ignore-revs`; the gate runs first in `clippy + tests`.
    C6 read replicas (verify against ADR-0010 first — the answer path may
    be closer to replicable than "strict lockstep" suggests); C7's restore
    drill is documented, rehearse it on the VM once; E3 artifact-evaluation
    container if the paper pursues a badge; periodic re-pin of the SHA-
    pinned CI actions. ~~ADR-0019's deferred hint caching when the UX
    asks~~ — **done 2026-07-28** (ADR-0038: IndexedDB persistence keyed by
    epoch, server-side `Range`/`If-Range` resume); the UX asked once the
    complete-set hint reached 553.82 MB, so this left the tail early.

## 3. Already right — do not "fix" these

- **Following `finalized` (ADR-0007)** — no reorg handling, on purpose.
- **Nonzero-only + verified fp ∧ `key_tag` ops (ADR-0015/0017)** —
  re-litigated once, re-affirmed; only new evidence reopens it.
- **`unsafe_code = "forbid"`** everywhere (one documented wasm FFI allow).
- **The pinned git dep** — never a path dep to the drifting local checkout.
- **Deny-by-default JSON-RPC (ADR-0012)** — the leaky default is the bug.

## 4. Scope discipline

Unchanged from v1: the PoC's value is that it is *honest*. Every item above
either makes an existing guarantee legible, enforced, or extended — nothing
gets built half-way to look production-ish. **C4** (the ability to see the
system) landed 2026-07-28; if only two more things get done from what
remains: **B3** (the audit that proves the panic-free claim) and **A3's
middle rung** (the last stretch of never-wrong-answer).
