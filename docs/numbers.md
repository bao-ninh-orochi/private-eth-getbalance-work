# RisePIR numbers table — Stage 3 (measured)

Machine: 8-core Apple Silicon, 16 GB RAM, target-cpu=native (.cargo/config.toml)
Date: 2026-07-22
Config: arity 3, bucket_size 4, fingerprint_bits 32, value = key_tag(32) ‖ balance(96) ‖ checksum(16) = 144 bits (ADR-0009), lwe_dim 1275 / sigma 6.4 (`SimpleConfig::default()`), mock seed 0xB0DAC0DE5CA1E000
Every number below is measured with `std::time::Instant` against a real, built `RisePirServer` — except the byte sizes in §4, which are computed from `Geometry::sizes` (deterministic, not timed).

**IKPIR build (read before reproducing).** The full-rebuild and answer-latency numbers here are measured against the workspace's pinned IKPIR `perf/optimized` rev (`3d60fa7`, 2026-07-21 — see the root `Cargo.toml`), with the default-on `parallel` feature (rayon matvec/GEMM kernels). A `--no-default-features` build reports substantially slower, single-threaded rebuild/answer times (the sizes and delta-byte figures are unaffected). `xtask bench` prints to stdout by default; pass `--write` to overwrite this file, and only do so from a build against the pinned rev — bump the rev and these numbers together, never separately.

## 1. Full-rebuild time (the headline denominator)

| accounts | full rebuild (measured) |
|---:|---:|
| 100,000 | 0.044 s |
| 1,000,000 | 0.392 s |
| 9,437,184 | 6.677 s |

## 2. Per-block patch time vs. mutations/block (K), at 1,000,000 accounts

Each point: 5 warm-up blocks discarded, then 10 measured blocks averaged (`docs/verification.md` Correction 4: N-independent in op count, plateaus once the hint exceeds cache — report what is actually seen).

| K (mutations/block) | patch time (ms/block, measured) |
|---:|---:|
| 50 | 1.8002 |
| 150 | 2.3088 |
| 300 | 3.5643 |
| 600 | 5.0571 |
| 1200 | 7.9530 |

## 3. Per-block delta bytes: compact vs. naive (K≈300, 1,000,000 accounts, realistic wei-scale balances)

| metric | value |
|---|---:|
| nonzero cells in delta | 2,724 |
| naive (10 B/cell, upstream `u16`+`i64`) | 26.60 KB (27,240 B) |
| compact (`BlockDelta::encoded_len`, varint/zigzag) | 7.93 KB (8,122 B) |
| compaction ratio | 3.35× |

## 4. Hint / query / response / A / server-DB sizes, and client memory

### 4a. Geometry, per scale (computed)

| accounts | num_buckets | plaintext_bits | load factor | cells/slot | row_width | k | R (reshape_rows) | C (reshape_row_width) |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 100,000 | 49,152 | 10 | 0.5086 | 18 | 72 | 15 | 1,093 | 1,080 |
| 1,000,000 | 393,216 | 9 | 0.6358 | 20 | 80 | 40 | 3,277 | 3,200 |
| 9,437,184 | 3,145,728 | 9 | 0.7500 | 20 | 80 | 114 | 9,199 | 9,120 |
| 200,503,969 (complete mainnet, 2026-07-26) | 100,663,296 | 8 | 0.4980 | 22 | 88 | 617 | 54,384 | 54,296 |

### 4b. Per-segment sizes, per scale (computed, not timed)

| accounts | hint/segment | query/segment | response/segment | A/segment | server DB |
|---:|---:|---:|---:|---:|---:|
| 100,000 | 5.25 MB (5,508,000 B) | 4.27 KB (4,372 B) | 4.22 KB (4,320 B) | 5.32 MB (5,574,300 B) | 13.50 MB (14,155,776 B) |
| 1,000,000 | 15.56 MB (16,320,000 B) | 12.80 KB (13,108 B) | 12.50 KB (12,800 B) | 15.94 MB (16,712,700 B) | 120.00 MB (125,829,120 B) |
| 9,437,184 | 44.36 MB (46,512,000 B) | 35.93 KB (36,796 B) | 35.62 KB (36,480 B) | 44.74 MB (46,914,900 B) | 960.00 MB (1,006,632,960 B) |
| 200,503,969 (complete mainnet) | 276.91 MB (276,909,600 B) | 217.54 KB (217,536 B) | 217.18 KB (217,184 B) | 277.36 MB (277,358,400 B) | 35.43 GB (35,433,480,192 B) |

### 4c. Deployment totals (×3 segments) and client memory (computed)

A client holds `A` + hint for every segment; server DB / hint / query / response / A above are already per-segment, so deployment totals and client memory both multiply by arity (3).

| accounts | hint total | query total | response total | A total | client memory (A+hint) total |
|---:|---:|---:|---:|---:|---:|
| 100,000 | 15.76 MB (16,524,000 B) | 12.81 KB (13,116 B) | 12.66 KB (12,960 B) | 15.95 MB (16,722,900 B) | 31.71 MB (33,246,900 B) |
| 1,000,000 | 46.69 MB (48,960,000 B) | 38.40 KB (39,324 B) | 37.50 KB (38,400 B) | 47.82 MB (50,138,100 B) | 94.51 MB (99,098,100 B) |
| 9,437,184 | 133.07 MB (139,536,000 B) | 107.80 KB (110,388 B) | 106.88 KB (109,440 B) | 134.22 MB (140,744,700 B) | 267.30 MB (280,280,700 B) |
| 200,503,969 (complete mainnet) | 830.73 MB (830,728,800 B) | 652.61 KB (652,608 B) | 651.55 KB (651,552 B) | 832.08 MB (832,075,200 B) | 1.66 GB (1,662,804,000 B) |

The last row is the honest cost of the complete set to a *client*: **830.73 MB
downloaded once** from `/setup`, and **1.66 GB resident** thereafter (the hint,
plus `A` re-expanded locally from its seed rather than transferred). That is the
inherent SimplePIR-class client footprint at 200 M accounts, and it is what
`docs/adr/0019` means when it says the browser front end gives way to the CLI
client at the complete set.

One caveat for the *browser* specifically: this table is steady state, and a
tab's real ceiling is the **init peak** — encoded bundle, decoded bundle, and
the built client transiently coexist, and wasm linear memory never shrinks, so
the peak is also the tab's floor from then on. After the init-sequence fixes
(free the encoded buffer between decode and build; consume decoded hints per
segment) that peak is ~2.4× the hint (~2.0 GB here), and the front end's
pre-flight budgets **3× the hint** for it (`ESTIMATED_PEAK_MULTIPLE`,
`web/pir.js` — derivation there; ADR-0032 revision). The CLI client's peak is
the same sequence minus the wasm no-shrink property.

## 5. Answer latency, at 1,000,000 accounts

| metric | value |
|---|---:|
| queries measured | 20 |
| avg `server.answer(&queries)` latency | 2.6845 ms |

## 6. The headline: full rebuild ÷ per-block patch (K≈300)

Duty cycle assumes a 12 s block (`docs/plan.md` §7's framing — the honest measured ratio, not the brief's 10^5–10^6).

| accounts | full rebuild | per-block patch (K≈300) | ratio (rebuild ÷ patch) | duty cycle @ 12s block |
|---:|---:|---:|---:|---:|
| 100,000 | 0.044 s | 1.9482 ms | 23× | 0.0162% |
| 1,000,000 | 0.392 s | 3.5643 ms | 110× | 0.0297% |
| 9,437,184 | 6.677 s | 4.9594 ms | 1346× | 0.0413% |

## 7. The complete mainnet set (200,503,969 accounts)

The live deployment (`docs/deploy.md` §5.3) serves the complete nonzero-balance mainnet set — 21x larger than this file's largest bench scale (9,437,184 accounts, §1–§6). This section states plainly what is and is not measured at that scale, and what a defensible extrapolation of the §6 headline ratio looks like. §1–§6 above are left exactly as measured 2026-07-22 — see the reproducibility note below for why.

**What is measured, and where.** The complete set's one-time full PIR-setup rebuild took **1236.5 s** at 200,503,969 accounts — `docs/deploy.md` §5.3, "PIR setup (one-time)", on the deployment host (GCP `e2-highmem-8`, 8 vCPU / 64 GB), **not** this benchmark machine. That is exactly §1's quantity (full-rebuild time) at deployment scale, but measured on a different machine from every other row in §1 and §6 — a fact that must travel with the number wherever it is quoted.

**What is not measured, plainly.** Per-block patch time has never been measured at the complete set. This laptop cannot hold that set — the geometry alone is a 35.43 GB server DB (§4b) — and the deployment box is a production server, not a benchmark rig, so it has never run the bench harness's warm-up/measured-block protocol either. §6 therefore has no 200,503,969-account row, and deliberately does not get one: a patch time nobody measured has no business next to five that were.

**What the trend shows.** A separate run on 2026-07-27 — "Run B" below, uncontaminated by competing builds, *not* the run behind §1–§6 — extended this harness past 9,437,184 accounts with this worktree's own `xtask bench --scales <n,n,...> --mid-scale <n>` flags. Its three largest points:

| accounts | full rebuild | per-block patch (K≈300) | ratio (rebuild ÷ patch) |
|---:|---:|---:|---:|
| 9,437,184 | 10.559 s | 6.7172 ms | 1572× |
| 18,874,368 | 23.503 s | 7.2875 ms | 3225× |
| 37,748,736 | 72.410 s | 14.6611 ms | 4939× |

Per-block patch time is *not* holding flat here — it grows from single-digit to ~15 ms across this range, the cache-plateau effect `docs/verification.md` Correction 4 already names, still climbing at these scales. §6's implicit hope that patch time stays near ~5 ms all the way to 200,503,969 accounts is not supported by this trend.

**The extrapolated ratio — EXTRAPOLATION, not a measurement.** The ratio itself, unlike either time alone, is to first order machine-independent: numerator and denominator both scale with this machine's own CPU speed, so a uniform machine slowdown (see the reproducibility note below) largely cancels out of their quotient. That is what makes extrapolating the *ratio* from Run B defensible where extrapolating either raw time alone would not be:

- Run B's ratio grows 1572× → 4939× from 9,437,184 to 37,748,736 accounts — a 4× increase in N producing a 3.14× increase in ratio, i.e. ratio ∝ N^0.83.
- Extending that exponent from 37,748,736 to the deployment's 200,503,969 (a 5.31× further increase in N) gives ratio ≈ 4939 × 5.31^0.83 ≈ EXTRAPOLATION **2 × 10^4** (on the order of 10^4).
- Run A (the contaminated run, not tabulated above) fits the same way to a smaller extrapolated ratio — consistent with treating this as an order-of-magnitude statement, not a precise one.
- Cross-check, itself EXTRAPOLATION: dividing the deployment's own measured 1236.5 s full rebuild by the ~2 × 10^4 extrapolated ratio implies a per-block patch of roughly 62 ms (55–75 ms, allowing for the extrapolation's own uncertainty) at 200,503,969 accounts on that host — a figure nobody has measured (see "instrumentation now exists" below).

**Honest summary.** The 1346× this file publishes for 9,437,184 accounts (§6) understates the argument at deployment scale by more than an order of magnitude; a 10^5 claim (the original brief's assumption) would overstate it. The defensible statement today is: **on the order of 10^4, and rising with N.**

**Reproducibility note.** Two runs on 2026-07-27 (Run A, contaminated by competing cargo builds; Run B, quoted above) came out uniformly 1.6–2× slower than the 2026-07-22 figures in §1–§5 across measurements that share no code path with the scale extension — e.g. answer latency at 1,000,000 accounts (5.5713 ms vs. the published 2.6845 ms, §5) and full-rebuild time at 9,437,184 accounts (10.559 s vs. the published 6.677 s, §1). That is a machine-state difference on this laptop, not a code change, so §1–§6 above were deliberately left at their measured 2026-07-22 values rather than overwritten with slower 2026-07-27 numbers — and this section's own figures are comparable to §1–§6 only in shape (the trend, the ratio), never in absolute terms.

**Instrumentation now exists.** `NodeState::apply_block` (`crates/risepir-http/src/node.rs`) now returns its own measured hint-patch duration, and the mainnet follow loop (`crates/risepir-rpc/src/mainnet.rs`) aggregates and periodically logs it with the mean mutations/block (K) over the same window. The next restart of the live deployment will start producing the one number this section still lacks — a directly measured complete-set patch time — with no extrapolation involved.
