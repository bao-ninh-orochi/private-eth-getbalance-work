# RisePIR numbers table — Stage 3 (measured)

Machine: 8-core Apple Silicon, 16 GB RAM, target-cpu=native (.cargo/config.toml)
Date: 2026-07-31
Config: arity 2, bucket_size 4, fingerprint_bits 32, value = key_tag(32) ‖ balance(96) ‖ checksum(16) = 144 bits (ADR-0009), lwe_dim 1275 / sigma 6.4 (`SimpleConfig::default()`), mock seed 0xB0DAC0DE5CA1E000
Every number below is measured with `std::time::Instant` against a real, built `RisePirServer` — except the byte sizes in §4, which are computed from `Geometry::sizes` (deterministic, not timed).

**IKPIR build (read before reproducing).** The full-rebuild and answer-latency numbers here are measured against the workspace's pinned IKPIR `perf/optimized` tag (`v0.1.0-perf` — see the root `Cargo.toml`), with the default-on `parallel` feature (rayon matvec/GEMM kernels). The tag replaced the `0f3b99b` rev these were originally measured on; its `crates/` tree is bit-identical to that rev, so the measurements carry over unchanged. A `--no-default-features` build reports substantially slower, single-threaded rebuild/answer times (the sizes and delta-byte figures are unaffected). `xtask bench` prints to stdout by default; pass `--write` to overwrite this file, and only do so from a build against the pinned tag — bump the pin and these numbers together, never separately.

## 1. Full-rebuild time (the headline denominator)

| accounts | full rebuild (measured) |
|---:|---:|
| 100,000 | 0.030 s |
| 1,000,000 | 0.704 s |
| 9,437,184 | 9.907 s |

## 2. Per-block patch time vs. mutations/block (K), at 1,000,000 accounts

Each point: 5 warm-up blocks discarded, then 10 measured blocks averaged (`docs/verification.md` Correction 4: N-independent in op count, plateaus once the hint exceeds cache — report what is actually seen).

| K (mutations/block) | patch time (ms/block, measured) |
|---:|---:|
| 50 | 1.8072 |
| 150 | 2.6516 |
| 300 | 3.1243 |
| 600 | 4.3205 |
| 1200 | 6.9657 |

## 3. Per-block delta bytes: compact vs. naive (K≈300, 1,000,000 accounts, realistic wei-scale balances)

| metric | value |
|---|---:|
| nonzero cells in delta | 2,724 |
| naive (10 B/cell, upstream `u16`+`i64`) | 27.24 KB (27,240 B) |
| compact (`BlockDelta::encoded_len`, varint/zigzag) | 8.12 KB (8,125 B) |
| compaction ratio | 3.35× |

## 4. Hint / query / response / A / server-DB sizes, and client memory

Every row below is computed from a [`Geometry`] — deterministic, never timed — the same as every other scale in `self.scales` (§1/§2/§3/§5/§6 additionally report real measurements at those scales, since a server was actually built there). The final row in each of 4a/4b/4c is different in kind, not just in size: it is `DEPLOYMENT_ACCOUNTS`, the live complete mainnet set (§7), and no server has ever been built at that scale on this machine — hence no corresponding row in §1/§2/§3/§5/§6, which report only what this run actually measured. Its geometry and sizes are derived exactly like every other row's, via `Geometry::for_accounts`/`Geometry::sizes` at this module's own `ARITY`/`BUCKET_SIZE` — pure arithmetic, not a measurement in disguise, and labelled below so it cannot be mistaken for one.

### 4a. Geometry, per scale (computed)

| accounts | num_buckets | plaintext_bits | load factor | cells/slot | row_width | k | R (reshape_rows) | C (reshape_row_width) |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 100,000 | 32,768 | 10 | 0.7629 | 18 | 72 | 15 | 1,093 | 1,080 |
| 1,000,000 | 524,288 | 9 | 0.4768 | 20 | 80 | 57 | 4,600 | 4,560 |
| 9,437,184 | 4,194,304 | 9 | 0.5625 | 20 | 80 | 162 | 12,946 | 12,960 |
| 201,059,658 (complete mainnet — computed, no server built at this scale) | 67,108,864 | 8 | 0.7490 | 22 | 88 | 617 | 54,384 | 54,296 |

### 4b. Per-segment sizes, per scale (computed, not timed)

| accounts | hint/segment | query/segment | response/segment | A/segment | server DB |
|---:|---:|---:|---:|---:|---:|
| 100,000 | 5.51 MB (5,508,000 B) | 4.37 KB (4,372 B) | 4.32 KB (4,320 B) | 5.57 MB (5,574,300 B) | 9.44 MB (9,437,184 B) |
| 1,000,000 | 23.26 MB (23,256,000 B) | 18.40 KB (18,400 B) | 18.24 KB (18,240 B) | 23.46 MB (23,460,000 B) | 167.77 MB (167,772,160 B) |
| 9,437,184 | 66.10 MB (66,096,000 B) | 51.78 KB (51,784 B) | 51.84 KB (51,840 B) | 66.02 MB (66,024,600 B) | 1.34 GB (1,342,177,280 B) |
| 201,059,658 (complete mainnet — computed, no server built at this scale) | 276.91 MB (276,909,600 B) | 217.54 KB (217,536 B) | 217.18 KB (217,184 B) | 277.36 MB (277,358,400 B) | 23.62 GB (23,622,320,128 B) |

### 4c. Deployment totals (×2 segments) and client memory (computed)

A client holds `A` + hint for every segment; server DB / hint / query / response / A above are already per-segment, so deployment totals and client memory both multiply by arity (2).

| accounts | hint total | query total | response total | A total | client memory (A+hint) total |
|---:|---:|---:|---:|---:|---:|
| 100,000 | 11.02 MB (11,016,000 B) | 8.74 KB (8,744 B) | 8.64 KB (8,640 B) | 11.15 MB (11,148,600 B) | 22.16 MB (22,164,600 B) |
| 1,000,000 | 46.51 MB (46,512,000 B) | 36.80 KB (36,800 B) | 36.48 KB (36,480 B) | 46.92 MB (46,920,000 B) | 93.43 MB (93,432,000 B) |
| 9,437,184 | 132.19 MB (132,192,000 B) | 103.57 KB (103,568 B) | 103.68 KB (103,680 B) | 132.05 MB (132,049,200 B) | 264.24 MB (264,241,200 B) |
| 201,059,658 (complete mainnet — computed, no server built at this scale) | 553.82 MB (553,819,200 B) | 435.07 KB (435,072 B) | 434.37 KB (434,368 B) | 554.72 MB (554,716,800 B) | 1.11 GB (1,108,536,000 B) |

The last row is the honest cost of the complete set to a *client*: **553.82 MB (553,819,200 B) downloaded once** from `/setup`, and **1.11 GB (1,108,536,000 B) resident** thereafter (the hint, plus `A` re-expanded locally from its seed rather than transferred). That is the inherent SimplePIR-class client footprint at 201,059,658 accounts, and it is what `docs/adr/0019` means when it says the browser front end gives way to the CLI client at the complete set.

One caveat for the *browser* specifically: this table is steady state, and a tab's real ceiling is the **init peak** — encoded bundle, decoded bundle, and the built client transiently coexist, and wasm linear memory never shrinks, so the peak is also the tab's floor from then on. After the init-sequence fixes (free the encoded buffer between decode and build; consume decoded hints per segment) that peak is ~2.4× the hint (~1.33 GB (1,329,166,080 B) here), and the front end's pre-flight budgets **3× the hint** for it (`ESTIMATED_PEAK_MULTIPLE`, `web/pir.js` — derivation there; ADR-0032 revision). The CLI client's peak is the same sequence minus the wasm no-shrink property.

## 5. Answer latency, at 1,000,000 accounts

| metric | value |
|---|---:|
| queries measured | 20 |
| avg `server.answer(&queries)` latency | 3.4095 ms |

## 6. The headline: full rebuild ÷ per-block patch (K≈300)

Duty cycle assumes a 12 s block (`docs/plan.md` §7's framing — the honest measured ratio, not the brief's 10^5–10^6).

| accounts | full rebuild | per-block patch (K≈300) | ratio (rebuild ÷ patch) | duty cycle @ 12s block |
|---:|---:|---:|---:|---:|
| 100,000 | 0.030 s | 1.4572 ms | 20× | 0.0121% |
| 1,000,000 | 0.704 s | 3.1243 ms | 225× | 0.0260% |
| 9,437,184 | 9.907 s | 3.9584 ms | 2503× | 0.0330% |

## 7. The complete mainnet set (201,059,658 accounts)

The live deployment (`docs/deploy.md` §5.3) serves the complete nonzero-balance mainnet set — 21x larger than this file's largest bench scale (9,437,184 accounts, §1–§6). This section states plainly what is and is not measured at that scale, and what a defensible extrapolation of the §6 headline ratio looks like. §1–§6 above are this run's own fresh measurements, never a value anyone hand-maintains to match them — see the reproducibility note below for how much run-to-run machine variance to expect before comparing them against any other figure this section cites.

**What is measured, and where.** The complete set's one-time full PIR-setup rebuild took **1236.5 s** at 201,059,658 accounts — `docs/deploy.md` §5.3, "PIR setup (one-time)", on the deployment host (GCP `e2-highmem-8`, 8 vCPU / 64 GB), **not** this benchmark machine, and under the pre-ADR-0034 `(arity 3, bucket_size 4)` lineage that host is still running (it has not yet been re-bootstrapped onto the `(arity 2, bucket_size 4)` geometry §1–§6 above measure). That is exactly §1's quantity (full-rebuild time) at deployment scale, but measured on a different machine, and a different geometry, from every other row in §1 and §6 — a fact that must travel with the number wherever it is quoted.

**What is not measured, plainly.** Per-block patch time has never been measured at the complete set. This laptop cannot hold that set — the deployed `(arity 2, bucket_size 4)` geometry alone is a 23.62 GB server DB (§4b; the live host was re-bootstrapped onto it on 2026-07-27 and now holds exactly that, in a 24.18 GB state file — deploy.md §5.4) — and the deployment box is a production server, not a benchmark rig, so it has never run the bench harness's warm-up/measured-block protocol either. §6 therefore has no 201,059,658-account row, and deliberately does not get one: a patch time nobody measured has no business next to five that were.

**What the trend shows.** A separate run on 2026-07-27 — "Run B" below, uncontaminated by competing builds, *not* the run behind §1–§6 — extended this harness past 9,437,184 accounts with this worktree's own `xtask bench --scales <n,n,...> --mid-scale <n>` flags, under the pre-ADR-0034 `(arity 3, bucket_size 4)` lineage this harness ran at the time (its `ARITY` constant has since moved to 2) — not the now-deployed `(arity 2, bucket_size 4)` geometry §1–§6 above measure. Its three largest points:

| accounts | full rebuild | per-block patch (K≈300) | ratio (rebuild ÷ patch) |
|---:|---:|---:|---:|
| 9,437,184 | 10.559 s | 6.7172 ms | 1572× |
| 18,874,368 | 23.503 s | 7.2875 ms | 3225× |
| 37,748,736 | 72.410 s | 14.6611 ms | 4939× |

Per-block patch time is *not* holding flat here — it grows from single-digit to ~15 ms across this range, the cache-plateau effect `docs/verification.md` Correction 4 already names, still climbing at these scales. §6's implicit hope that patch time stays near ~5 ms all the way to 201,059,658 accounts is not supported by this trend.

**The extrapolated ratio — EXTRAPOLATION, not a measurement.** The ratio itself, unlike either time alone, is to first order machine-independent: numerator and denominator both scale with this machine's own CPU speed, so a uniform machine slowdown (see the reproducibility note below) largely cancels out of their quotient. That is what makes extrapolating the *ratio* from Run B defensible where extrapolating either raw time alone would not be:

- Run B's ratio grows 1572× → 4939× from 9,437,184 to 37,748,736 accounts — a 4× increase in N producing a 3.14× increase in ratio, i.e. ratio ∝ N^0.83.
- Extending that exponent from 37,748,736 to the deployment's 201,059,658 (a 5.33× further increase in N) gives ratio ≈ 4939 × 5.33^0.83 ≈ EXTRAPOLATION **2 × 10^4** (on the order of 10^4).
- Run A (the contaminated run, not tabulated above) fits the same way to a smaller extrapolated ratio — consistent with treating this as an order-of-magnitude statement, not a precise one.
- Cross-check, itself EXTRAPOLATION: dividing the deployment's own measured 1236.5 s full rebuild by the ~2 × 10^4 extrapolated ratio implies a per-block patch of roughly 62 ms (55–75 ms, allowing for the extrapolation's own uncertainty) at 201,059,658 accounts on that host — a figure nobody has measured (see "instrumentation now exists" below).

**Honest summary.** The 2503× this file publishes for 9,437,184 accounts (§6) understates the argument at deployment scale by roughly 8×; a 10^5 claim (the original brief's assumption) would overstate it. The defensible statement today is: **on the order of 10^4, and rising with N.**

**Reproducibility note.** Two historical runs on 2026-07-27 — Run A (contaminated by competing cargo builds) and Run B (quoted above), both this module's pre-ADR-0034 `(arity 3, bucket_size 4)` — measured answer latency at 1,000,000 accounts (5.5713 ms) and full-rebuild time at 9,437,184 accounts (10.559 s), uniformly slower than the 2026-07-22 `(3,4)` baseline this file used to publish (2.6845 ms and 6.677 s respectively — fixed historical citations, not this file's own current §1/§5, which may since be a different geometry and a different day). This run's own figures at the same two scales — 9.907 s (§1) and 3.4095 ms (§5) — are a third, independent data point in that same run-to-run variance. That gap is machine-state variance on this laptop, not a code change — which is exactly why §1–§6 above are always this run's own fresh measurements rather than a value anyone hand-maintains to match them: comparing any two runs of this file is meaningful only in *shape* (the trend, the ratio), never in absolute terms. See the same-machine control below for a same-day, same-machine comparison that isolates the geometry's own effect from exactly this kind of variance.

**Same-machine control (2026-07-27).** §1–§6 above are this run's own fresh measurements at the now-deployed `(arity 2, bucket_size 4)` geometry (ADR-0034) — a different geometry, and very possibly a different day, from the 2026-07-22 `(arity 3, bucket_size 4)` table this file used to publish before ADR-0034, so comparing the two naively conflates both changes at once. To separate them, this laptop ran both configurations back to back, otherwise idle, at `BenchConfig::default()`'s three scales — measured *before* the run that produced §1–§6 above, so this control's own `(2,4)` column differs from whatever §1/§5/§6 actually show by however much the machine's load changed between the two runs, not by a code or geometry change: exactly the run-to-run variance this control exists to expose, not a discrepancy to reconcile.

| accounts | (2,4) rebuild | (2,4) patch (K≈300) | (2,4) ratio | (3,4) rebuild | (3,4) patch (K≈300) | (3,4) ratio |
|---:|---:|---:|---:|---:|---:|---:|
| 100,000 | 0.037 s | 2.4493 ms | 15× | 0.066 s | 2.8398 ms | 23× |
| 1,000,000 | 0.973 s | 4.6408 ms | 210× | 0.569 s | 4.8994 ms | 116× |
| 9,437,184 | 12.797 s | 5.2321 ms | 2446× | 8.984 s | 6.8111 ms | 1319× |

(§5's exact measurement, answer latency @ 1,000,000 accounts: 5.5062 ms at `(2,4)`, 4.3058 ms at `(3,4)`.)

Three things this control establishes, holding the machine fixed:

- The machine is still in a slow state: today's `(3,4)` control latency (4.3058 ms) and the 2026-07-27 Run B figure (5.5713 ms, above) are both well above the 2026-07-22 `(3,4)` baseline this file used to publish (2.6845 ms) — clear evidence of how much this laptop's run-to-run timing varies on its own, independent of any code or geometry change.
- The ratio is machine-state-robust — direct evidence for the reproducibility note's "the ratio largely cancels a uniform machine slowdown" argument above: every absolute time in the control table runs roughly 1.5× slower than that same 2026-07-22 baseline, yet the `(3,4)` control's 1319× ratio at 9,437,184 accounts lands within 2.0% of the baseline's own 1346× — the ratio held even though nothing else did.
- At these three bench scales `(2,4)` is the *unfavourable* case, and the committed numbers therefore understate the deployed geometry — a reader must not conclude the arity change itself made this system 1.42× slower. Arity 2's power-of-two quantization lands badly at exactly 9,437,184 accounts: `(3,4)` needs 3,145,728 buckets (load 0.7500, 251,658,240 cells) where `(2,4)` needs 4,194,304 (0.5625 load, 335,544,320 cells — 1.33× more data), which is why the `(2,4)` control rebuilds 1.42× slower here (12.797 s vs. 8.984 s) — a consequence of the account count landing awkwardly for this arity's quantization at this particular scale, not of the arity change in general. At the complete set the relationship inverts: `(2,4)`'s server DB is 23.62 GB (23,622,320,128 B) against `(3,4)`'s 35.43 GB (35,433,480,192 B) — 1.50× *fewer* cells at deployment scale. The one genuine arity effect visible in the control table runs the other way from rebuild time: per-block patch time is *lower* at `(2,4)` (5.2321 ms vs. 6.8111 ms at 9,437,184 accounts) even though that scale holds more data under `(2,4)` — fewer segments (2, not 3) to patch.

**Instrumentation now exists, and it has now reported — the extrapolation above was pessimistic by 5-7x.** `NodeState::apply_block` (`crates/risepir-http/src/node.rs`) returns its own measured hint-patch duration, and the mainnet follow loop (`crates/risepir-rpc/src/mainnet.rs`) aggregates and periodically logs it with the mean mutations/block (K) over the same window. The 2026-07-31 re-bootstrap (`docs/deploy.md` §5.8) produced the number this section was written without, on the deployment host at the complete set:

| regime | mean | min | max | mean K |
|---|---:|---:|---:|---:|
| during catch-up replay | 8.23 ms -> 8.81 ms | 2.51 ms | 20.14 ms | 311-326 |
| **once following the head** | **11.09 ms -> 11.11 ms** | 0.66 ms | 29.20 ms | 303-323 |

Two windows in each regime, agreeing closely — a stable figure, not one lucky sample — and at K ~ 300-326, the same mutation rate §2/§6 bench at, so it is directly comparable rather than a different workload. Against the ~62 ms (55-75 ms) extrapolated just above, the measured **~11.1 ms** is 5-7x better, which runs the other way through §6's arithmetic: dividing the deployment's own 1236.5 s rebuild by a *measured* 11.1 ms puts the rebuild ÷ patch ratio on the order of **10^5**, not the ~2 x 10^4 extrapolated. The honest summary above therefore **understates** the deployed case; it is kept as written because it was the defensible statement from the data then available, and because the two halves of that quotient still are not measured the same way — 1236.5 s is the pre-ADR-0034 `(3,4)` rebuild, 11.1 ms is today's `(2,4)` deployment. A same-geometry pair for both halves does not exist yet, so the 10^5 is an *improved estimate*, not a measured ratio.

Named rather than buried: the catch-up figures are taken while the server applies blocks back to back, touching the hint far more densely in time than following the head does. That is the likeliest reason catch-up looks *faster* than steady state (a warmer cache), and it is why the following-head row is the one to quote.
