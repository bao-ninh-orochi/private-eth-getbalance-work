# RisePIR numbers table — Stage 3 (measured)

Machine: 8-core Apple Silicon, 16 GB RAM, target-cpu=native (.cargo/config.toml)
Date: 2026-07-17
Config: arity 3, bucket_size 4, fingerprint_bits 32, value = key_tag(32) ‖ balance(96) ‖ checksum(16) = 144 bits (ADR-0009), lwe_dim 1275 / sigma 6.4 (`SimpleConfig::default()`), mock seed 0xB0DAC0DE5CA1E000
Every number below is measured with `std::time::Instant` against a real, built `RisePirServer` — except the byte sizes in §4, which are computed from `Geometry::sizes` (deterministic, not timed).

**IKPIR build (read before reproducing).** The full-rebuild and answer-latency numbers here are measured against the IKPIR `perf/optimized` tree (`042d868`) — the default-on rayon-parallel matvec/GEMM config that `docs/plan.md` §7 and `docs/verification.md` report against, and this project's intended production dependency (the "switch the path deps to the pinned git dep `rev = 042d868`" open item). The repo's current local path-dep points at `main` (`3287c39`), which lacks the parallel kernels, so an `xtask bench` run against that build reports substantially slower, single-threaded rebuild/answer times (the §3 delta-byte and §4 size figures are unaffected). `xtask bench` prints to stdout by default; pass `--write` to overwrite this file, and only do so from a build against `042d868`.

## 1. Full-rebuild time (the headline denominator)

| accounts | full rebuild (measured) |
|---:|---:|
| 100,000 | 0.041 s |
| 1,000,000 | 0.386 s |
| 9,437,184 | 6.225 s |

## 2. Per-block patch time vs. mutations/block (K), at 1,000,000 accounts

Each point: 5 warm-up blocks discarded, then 10 measured blocks averaged (`docs/verification.md` Correction 4: N-independent in op count, plateaus once the hint exceeds cache — report what is actually seen).

| K (mutations/block) | patch time (ms/block, measured) |
|---:|---:|
| 50 | 1.2336 |
| 150 | 1.3335 |
| 300 | 2.4159 |
| 600 | 3.1380 |
| 1200 | 4.9744 |

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

### 4b. Per-segment sizes, per scale (computed, not timed)

| accounts | hint/segment | query/segment | response/segment | A/segment | server DB |
|---:|---:|---:|---:|---:|---:|
| 100,000 | 5.25 MB (5,508,000 B) | 4.27 KB (4,372 B) | 4.22 KB (4,320 B) | 5.32 MB (5,574,300 B) | 13.50 MB (14,155,776 B) |
| 1,000,000 | 15.56 MB (16,320,000 B) | 12.80 KB (13,108 B) | 12.50 KB (12,800 B) | 15.94 MB (16,712,700 B) | 120.00 MB (125,829,120 B) |
| 9,437,184 | 44.36 MB (46,512,000 B) | 35.93 KB (36,796 B) | 35.62 KB (36,480 B) | 44.74 MB (46,914,900 B) | 960.00 MB (1,006,632,960 B) |

### 4c. Deployment totals (×3 segments) and client memory (computed)

A client holds `A` + hint for every segment; server DB / hint / query / response / A above are already per-segment, so deployment totals and client memory both multiply by arity (3).

| accounts | hint total | query total | response total | A total | client memory (A+hint) total |
|---:|---:|---:|---:|---:|---:|
| 100,000 | 15.76 MB (16,524,000 B) | 12.81 KB (13,116 B) | 12.66 KB (12,960 B) | 15.95 MB (16,722,900 B) | 31.71 MB (33,246,900 B) |
| 1,000,000 | 46.69 MB (48,960,000 B) | 38.40 KB (39,324 B) | 37.50 KB (38,400 B) | 47.82 MB (50,138,100 B) | 94.51 MB (99,098,100 B) |
| 9,437,184 | 133.07 MB (139,536,000 B) | 107.80 KB (110,388 B) | 106.88 KB (109,440 B) | 134.22 MB (140,744,700 B) | 267.30 MB (280,280,700 B) |

## 5. Answer latency, at 1,000,000 accounts

| metric | value |
|---|---:|
| queries measured | 20 |
| avg `server.answer(&queries)` latency | 2.5171 ms |

## 6. The headline: full rebuild ÷ per-block patch (K≈300)

Duty cycle assumes a 12 s block (`docs/plan.md` §7's framing — the honest measured ratio, not the brief's 10^5–10^6).

| accounts | full rebuild | per-block patch (K≈300) | ratio (rebuild ÷ patch) | duty cycle @ 12s block |
|---:|---:|---:|---:|---:|
| 100,000 | 0.041 s | 1.1468 ms | 35× | 0.0096% |
| 1,000,000 | 0.386 s | 2.4159 ms | 160× | 0.0201% |
| 9,437,184 | 6.225 s | 2.9683 ms | 2097× | 0.0247% |
