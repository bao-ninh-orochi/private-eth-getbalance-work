# Deployment numbers — the live complete-set RisePIR server, measured from inside (2026-09-03)

Sibling of [`docs/numbers.md`](numbers.md) (the offline `xtask bench` harness on a laptop). Everything here was
**measured on the live deployment in one campaign**: one binary at one commit, one geometry, one host, one
contiguous observation window. No figure is inherited from `docs/numbers.md`, `docs/deploy.md`, or any earlier
run. Every row is labelled **measured** (a clock or a byte count on a real call), **computed** (closed-form from
the geometry via `risepir_proto::Geometry::sizes`), or **derived** (arithmetic on other rows). Nothing is
extrapolated; a quantity that could not be measured is listed as absent with the reason. Raw data:
[`docs/data/deployment-20260903/`](data/deployment-20260903/) — one row per private query, one row per delta
fetch, one row per applied block, the `time-setup` JSON, the end-of-window `/metrics` and `/healthz` snapshots.
Every statistic below is recomputable from those files alone with `cargo run -p xtask --release -- report …`
(the exact command is §7). All times are UTC.

## 0. Provenance

| item | value |
|---|---|
| commit | `b37e4eedc82477b5df5dd2bdf65c47ddab1e0a4f` (`b37e4ee`, upstream `main`, the PR #8 merge) — server, probe and `xtask report` all built from this tree |
| primitive | `bao-ninh-orochi/IKPIR` tag `v0.2.0-perf` (`d91c75f`), RisePIR-S (SimplePIR/LWE backend) |
| geometry | arity 2, bucket_size 4, 67,108,864 buckets, `plaintext_bits` 8, LWE dimension n = 1275, fingerprint 32 bits + `key_tag` (ADR-0034 lineage, `RPST3`) |
| server host | GCP `c3d-highmem-16` (`risepir-c3d`), us-east4-a: AMD EPYC 9B14 (Zen 4, "Genoa"), 8 physical cores / 16 vCPU, 125 GiB RAM, 250 GB pd-balanced; Debian 12.15, kernel 6.1.0-52-cloud-amd64, glibc 2.36; `rustc 1.96.0`, `target-cpu=native`, rayon 16 threads |
| server binary | sha256 `77e2f06e9a2888c1b6b00d25f0628ee9c7f4cb0db2d7b71feafc5f62794e85fd` |
| cache topology | L1d 32 KiB ×8, L2 1 MiB ×8, L3 32 MiB shared by all 16 vCPUs, per `lscpu` (raw output: [`lscpu-server.txt`](data/deployment-20260903/lscpu-server.txt), captured on the identical-type replacement instance after the measurement host itself was deleted — its header explains why). On AMD instances `lscpu` reports L3 pro-rata to the vCPU count; 8 cores is exactly one Genoa CCD, so the pro-rata figure and the physical CCD coincide here. The 553.82 MB hint is ~17× L3 either way — the per-block patch runs out of cache, not in it (`docs/verification.md` Correction 4) |
| client vantage | AWS `r7a.xlarge` (`i-08a770e1d6e8a180b`), us-east-1b: AMD EPYC 9R14 (Zen 4), 4 vCPU, 16 MiB L3 share, 30 GiB; Ubuntu 24.04, kernel 7.0.0-1011-aws; `rustc 1.96.0`, `target-cpu=native`. The paper's own evaluation instance type. Probe binary sha256 `553aa8df00489eaf81bae436f16ea9c7c10a204563afd52a1947c57fac88afad` |
| link | public internet, cloud-to-cloud (AWS us-east-1 → GCP us-east4, both Northern Virginia); TLS 1.3 via `rustls`, terminated by Caddy on the host; HTTP/1.1 keep-alive on one `reqwest` client for the whole run; `demo.risepir.org` resolved client-side to `35.199.37.209` (`--resolve`), certificate validation left on |
| feed / reconcile | `https://eth.drpc.org` (+ `https://eth.merkle.io` fallback), `prestateTracer`; independent provider for both the probe's per-trial correctness check and the server's reconcile loop: `https://ethereum-rpc.publicnode.com` (ADR-0007: comparisons at an explicit block height, `"latest"` = finalized) |
| window | 2026-09-03T00:46:45Z (probe start) → 2026-09-03T03:54:37Z (last delta ingested) — 3.13 h; server rows 25,893,198–25,894,156 (959 blocks applied while the session was live); probe session pinned at block₀ = 25,893,197; answers at blocks 25,893,197–25,894,124 |
| samples | 300 private queries in 3 batches (300 successful, 0 errored), 125 delta fetches covering 959 blocks, 959 server block rows, 1 `time-setup` run |
| collected | 2026-09-03T04:05:31Z |

**Method.** Percentiles are nearest-rank on the sorted successful samples: for quantile *q* over *n* samples the
reported value is the sample at 1-based rank ⌈*q·n*⌉ — an actual sample, never interpolated. *n* counts only
trial rows whose `error` column is empty; a nonempty `error` excludes the row from every §2 statistic. The
block CSVs carry no `error` column, so their *n* is every parsed row.

## 1. The client staleness operating point (read this before A5)

One probe session for the whole window, created from one `/setup` download at **block₀ = 25,893,197**, and
**never garbage-collected** — exactly what the shipped CLI client and the browser client do (neither calls
`collect_garbage`; ADR-0003, ADR-0048). The session follows head between and inside batches (`/head` every
12 s, `/sync` for new blocks), so its pending delta ΔD spans block₀ → the answered block and grows through the
window:

| batch | first trial (UTC) | n | `stale_blocks`, first → last trial | `delta_cells`, first → last trial |
|---|---|---:|---:|---:|
| 0 | 00:46:47 | 100 | 0 → 16 | 0 → 39,525 |
| 1 | 02:16:47 | 100 | 447 → 479 | 527,882 → 559,855 |
| 2 | 03:46:47 | 100 | 895 → 927 | 990,428 → 1,018,161 |

Over all 300 successful trials (measured): `pinned_block` is 25,893,197 on every row; `stale_blocks` ranges
**0–927**; `delta_cells` ranges **0–1,018,161**. A5 is Θ(|ΔD|) — §2.5 reports it binned by staleness. A
browser user who keeps a cached hint for days (ADR-0038) sits far to the right of these bins.

## 2. A — one private query

Headline, in words: a private `eth_getBalance` against the complete 204.7 M-account mainnet set costs the
client **689.8 ms** end to end from a Virginia cloud vantage, of which **609.6 ms** is the server's own PIR
answer compute and **22.0 ms** is network. Tables below are in the units `xtask report` printed them in
(microseconds for the client-side legs, milliseconds for the server compute), verbatim.

### 2.1 A1–A2, the wire legs, and A5 (measured)

| component | n | mean (µs) | p50 (µs) | p95 (µs) | min (µs) | max (µs) |
|---|---:|---:|---:|---:|---:|---:|
| A1 — `t_total_us` (whole trial) | 300 | 689819.01 | 689536.00 | 699706.00 | 679483.00 | 711390.00 |
| A2 — `build_us` (query build) | 300 | 35015.11 | 34938.00 | 35273.00 | 34687.00 | 39110.00 |
| `head_wire_us` | 300 | 2605.46 | 2593.00 | 2666.00 | 2124.00 | 8437.00 |
| `sync_wire_us` | 300 | 471.20 | 0.00 | 2797.00 | 0.00 | 5268.00 |
| `answer_wire_us` (POST `/answer`) | 300 | 628802.46 | 628700.00 | 631117.00 | 625974.00 | 642040.00 |
| A5 — `finish_us` (rewind + decode) | 300 | 22348.85 | 22031.00 | 30360.00 | 14764.00 | 30870.00 |

### 2.2 The budget identity, closed with the residual (derived)

`A1 = A2 + head_wire + sync_wire + answer_wire + setup_wire + A5 + residual`, with `residual` **defined as the
subtraction** — never distributed, never absorbed. `setup_wire_us` is 0 on every row because the session was
created once, before the first trial, and never re-bootstrapped.

| component | mean (µs) |
|---|---:|
| A2 — `build_us` | 35015.11 |
| `head_wire_us` | 2605.46 |
| `sync_wire_us` | 471.20 |
| `answer_wire_us` | 628802.46 |
| `setup_wire_us` | 0.00 |
| A5 — `finish_us` | 22348.85 |
| `residual_us` (client bookkeeping) | 575.92 |
| **sum of components** (derived) | **689819.01** |
| A1 — `t_total_us` mean, for comparison | 689819.01 |

Per-row check (derived): max |A1 − (A2 + head + sync + answer + setup + A5 + residual)| across all 300
successful rows = **0 µs**. The identity holds exactly, row by row, not just in the mean.

### 2.3 A3 (network) and A4 (server compute)

A4 is the server's own measured PIR answer compute, returned in a response header (`--answer-timing-header`,
ADR-0048) and therefore *measured*, not inferred. Timing headers were present on 300/300 successful rows.

**A4 (measured):**

| quantity | n | mean (ms) | p50 (ms) | p95 (ms) | min (ms) | max (ms) |
|---|---:|---:|---:|---:|---:|---:|
| A4 — `server_compute_ns` | 300 | 609.5756 | 609.6180 | 611.4418 | 607.0005 | 616.2947 |

**A3 and the server's handler overhead (derived).** A3 is *stated as a derivation*: the wire time of every
network call in the trial minus the server's own reported per-request handler time. It is that path's RTT plus
transfer, not a property of the server.

| quantity | n | mean (µs) | p50 (µs) | p95 (µs) | min (µs) | max (µs) |
|---|---:|---:|---:|---:|---:|---:|
| A3 = (head + sync + answer wire) − `server_handler_ns` | 300 | 21976.55 | 21297.01 | 24167.86 | 20898.23 | 31080.14 |
| server handler overhead = handler − A4 (compute) | 300 | 327.01 | 246.22 | 333.63 | 203.79 | 9254.80 |

Cross-check from the server's own histogram at collection (`metrics-end.txt`, measured): 439 `/answer` calls
over the whole server process — the campaign's 300 plus the smoke runs and the abandoned residential run of §4.4
— `risepir_answer_duration_seconds_sum` 267.6601666910002 s, i.e. 609.7 ms mean, every one of them in the
`le="1"` bucket and none below `le="0.5"`.

### 2.4 A5 sub-timers (measured)

The four ADR-0003 rewind steps, as the client actually runs them.

| sub-timer | n | mean (µs) | p50 (µs) | p95 (µs) | min (µs) | max (µs) |
|---|---:|---:|---:|---:|---:|---:|
| `rewind_us` | 300 | 7475.10 | 7155.00 | 15498.00 | 0.00 | 15962.00 |
| `decode_us` | 300 | 14860.00 | 14856.00 | 14925.00 | 14755.00 | 16405.00 |
| `delta_apply_us` | 300 | 5.10 | 6.00 | 8.00 | 0.00 | 21.00 |
| `scan_us` | 300 | 3.65 | 4.00 | 4.00 | 2.00 | 14.00 |

Sum of the sub-timer means (derived): 22343.84 µs, against A5's own mean of 22348.85 µs — a difference of
−5.01 µs, the un-timed glue between the steps.

### 2.5 A1 and A5 binned by staleness (measured)

Bin edges in blocks of staleness: 0–99, 100–299, 300–599, 600–899, 900+. The 100–299 bin is empty because the
three batches sample staleness ≈0 / ≈450 / ≈900 by design (§1), not because anything failed there.

**A1 — `t_total_us`:**

| bin | n | mean (µs) | p50 (µs) | p95 (µs) |
|---|---:|---:|---:|---:|
| 0–99 | 100 | 682360.06 | 681629.00 | 686959.00 |
| 100–299 | 0 | — | — | — |
| 300–599 | 100 | 690452.44 | 689453.00 | 696344.00 |
| 600–899 | 59 | 695506.64 | 695161.00 | 699811.00 |
| 900+ | 41 | 698281.95 | 697804.00 | 702048.00 |

**A5 — `finish_us`:**

| bin | n | mean (µs) | p50 (µs) | p95 (µs) |
|---|---:|---:|---:|---:|
| 0–99 | 100 | 14868.48 | 14827.00 | 15129.00 |
| 100–299 | 0 | — | — | — |
| 300–599 | 100 | 22107.92 | 22031.00 | 22554.00 |
| 600–899 | 59 | 29872.97 | 29873.00 | 30064.00 |
| 900+ | 41 | 30353.90 | 30338.00 | 30683.00 |

A5 doubles from 14.9 ms to 30.4 ms across the window; A1 moves only 682.4 → 698.3 ms, because A4 dominates it.
Staleness is the client's cost, and it is the one cost a client controls (re-download the hint, or don't).

### 2.6 A6 — wire bytes, measured against computed (measured, computed)

| field | n | mean | min | max |
|---|---:|---:|---:|---:|
| `query_bytes` (client → server) | 300 | 435.08 KB (435,083 B) | 435.08 KB (435,083 B) | 435.08 KB (435,083 B) |
| `response_bytes` (server → client) | 300 | 434.39 KB (434,387 B) | 434.39 KB (434,387 B) | 434.39 KB (434,387 B) |
| `response_content_length` | 300 | 434.39 KB (434,387 B) | 434.39 KB (434,387 B) | 434.39 KB (434,387 B) |

All three are constant across every successful trial (min == max), as a fixed private-query geometry requires.
A query costs the same bytes whatever it asks — that is the point.

Computed from `Geometry::sizes` at the `--setup` geometry (204,713,227 accounts, arity 2, 67,108,864 buckets,
`plaintext_bits` 8), assuming the measured byte counts cover all `arity` segments of one query:

| field | measured mean | computed total (computed) |
|---|---:|---:|
| query bytes | 435.08 KB (435,083 B) | 435.07 KB (435,072 B) |
| response bytes | 434.39 KB (434,387 B) | 434.37 KB (434,368 B) |

The 11 B and 19 B gaps are the protocol framing the geometry does not model.

## 3. B — one block

### 3.1 B7 — per-block mutation counts (measured)

| metric | n | mean | p50 | p95 | min | max | total |
|---|---:|---:|---:|---:|---:|---:|---:|
| inserts | 959 | 26.10 | 18 | 96 | 0 | 128 | 25,032 |
| updates | 959 | 400.21 | 326 | 943 | 41 | 1947 | 383,803 |
| deletes | 959 | 6.94 | 6 | 15 | 0 | 52 | 6,652 |
| `noop_deletes` | 959 | 0.07 | 0 | 1 | 0 | 3 | 67 |
| changes | 959 | 417.32 | 340 | 959 | 25 | 1991 | 400,210 |
| credits | 959 | 16.00 | 16 | 16 | 16 | 16 | 15,344 |
| `touched_cells` | 959 | 3559.46 | 3174 | 7147 | 284 | 14039 | 3,413,523 |

The K the rest of this repo quotes is changes + credits (derived): mean **433.3**, p50 356, p95 975. Credits
are exactly 16 on every block — the withdrawal credits, one per validator-payout slot.

### 3.2 B8 — `apply_ms`, by interference subset (measured)

Four subsets, because two things could have perturbed the follow loop during the window: the probe's own
answers, and the C13 `time-setup` run (§4.3), which held 25.8 GB and one busy core on the same host from
00:49:43 to 01:42:29. Neither is visible.

| subset | n | mean (ms) | p50 (ms) | p95 (ms) | min (ms) | max (ms) |
|---|---:|---:|---:|---:|---:|---:|
| all blocks | 959 | 5.5731 | 4.7830 | 10.9750 | 1.5060 | 21.4170 |
| quiet (`answers_since_prev_block == 0`, outside `time-setup`) | 651 | 5.6255 | 4.6680 | 12.3580 | — | — |
| probe-adjacent (`answers_since_prev_block > 0`) | 52 | 5.5015 | 4.7560 | 11.2980 | 1.8200 | 12.8500 |
| during `time-setup` | 256 | 5.4543 | 4.9980 | 9.6280 | — | — |

Probe-adjacent blocks are *faster* than quiet ones in the mean, which is the honest way of saying the
interference is below the noise floor. `xtask report`'s own `quiet` row is the union of rows 2 and 4
(`answers_since_prev_block == 0`, n = 907: mean 5.5772, p50 4.7840, p95 10.9150 ms); this table splits it so
the `time-setup` overlap is separately visible. No row has an empty `answers_since_prev_block`.

### 3.3 B8 — stage breakdown (measured, derived)

| stage | n | mean (ms) | p50 (ms) | p95 (ms) | min (ms) | max (ms) |
|---|---:|---:|---:|---:|---:|---:|
| `store_ms` | 959 | 1.2424 | 1.0870 | 2.4930 | 0.1530 | 4.5550 |
| `fold_ms` | 959 | 0.2875 | 0.2430 | 0.5930 | 0.0510 | 1.3150 |
| `patch_ms` | 959 | 3.9860 | 3.4340 | 7.7860 | 1.0690 | 15.4750 |
| residual = `apply_ms` − (store + fold + patch) (derived) | 959 | 0.0572 | 0.0440 | 0.1540 | 0.0030 | 0.4100 |
| `lock_wait_ms` | 959 | 3.5168 | 0.0010 | 0.0010 | 0.0000 | 529.4870 |

`patch_ms` — the hint patch, the quantity this whole system exists to keep small — is 3.99 ms mean at
K̄ = 433, against a 29.18 s full setup (§4.3). `lock_wait_ms` is the one row with a fat tail: p50 and p95 are
both 1 µs, but the max is 529 ms, which is the follow loop's write lock queued behind an in-flight ~610 ms
answer during a probe batch. That wait is not `apply_ms`; it is the block waiting its turn, and it happens only
while a query is being answered.

### 3.4 B9 — delta bytes, server against client (measured, derived)

| source | n | mean | p50 | p95 | min | max |
|---|---:|---:|---:|---:|---:|---:|
| server `delta_bytes` (per block) | 959 | 10.81 KB (10,812 B) | 9.49 KB (9,486 B) | 22.37 KB (22,367 B) | 871 B | 44.46 KB (44,458 B) |
| client `wire_bytes` (single-block fetches only) | 30 | 11.61 KB (11,609 B) | 11.88 KB (11,879 B) | 24.84 KB (24,842 B) | 2.48 KB (2,485 B) | 25.31 KB (25,311 B) |

derived: mean(client `wire_bytes`, single-block fetches) − mean(server `delta_bytes`, all blocks) =
**796.9 B**. The two subsets are not the same blocks, so read that as the scale of the HTTP framing, not as a
per-block delta between the two measurements.

### 3.5 B10 — client ingest and decode: per fetch (measured), per block (derived)

**The unit here is a fetch, not a block, and the difference is not cosmetic.** The server follows `finalized`
(ADR-0007), which advances in epoch-sized jumps, so a client that polls every 12 s coalesces whatever arrived
since its last fetch: 125 fetches covered 959 blocks in this window, `blocks_in_fetch` mean 7.672, p50 3, p95
26, min 1, max 31. Per-fetch rows below are **measured**; per-block rows are **derived** — the column total
divided by the 959 blocks those fetches covered — and are an average over a coalesced fetch, not a measurement
of one block's cost.

**How Ethereum's cadence produces this (read before quoting B10).** The chain's head grows one block per
12 s slot, but this deployment follows the *finalized* head (ADR-0007). Finality is decided per 32-slot epoch
(6.4 min): `finalized` sits 64–95 blocks behind the tip and moves forward by about 32 blocks at once, roughly
every 6.4 min. The server then applies those blocks strictly one at a time, each needing its own feed call to
dRPC (1–2 s at the head, where there is nothing to prefetch), so the server's own head creeps up over the
following 30–60 s. A client polling `GET /head` every 12 s catches that progress part-way and asks `GET /sync`
for whatever arrived since its last poll, which the server answers as one merged delta. The fetch sizes (1–31
blocks, mean 7.7) are therefore a product of the poll cadence and the server's apply pacing, not a property of
Ethereum, and the fetch count (125) is emergent — about 29 epoch boundaries in the window, each picked up in
about four fetches — not a parameter that was chosen. The server-side per-block figures (B7–B9) are unaffected:
every block is applied individually whichever head is followed; finality only changes *when* a batch arrives.
A true per-block client distribution would need a probe that fetches `GET /delta/{block}` block by block (the
route exists); this campaign did not, so the per-block client rows below stay derived.

| quantity | n | mean | p50 | p95 | min | max |
|---|---:|---:|---:|---:|---:|---:|
| **per fetch (measured)** — `wire_bytes` | 125 | 60.52 KB (60,524.4 B) | 30.77 KB (30,774 B) | 183.54 KB (183,541 B) | 2.48 KB (2,485 B) | 287.51 KB (287,509 B) |
| per fetch — `fetch_wire_us` | 125 | 6250.9 µs | 4412 µs | 15259 µs | 2112 µs | 18319 µs |
| per fetch — `ingest_us` | 125 | 2229.6 µs | 1428 µs | 6524 µs | 60 µs | 9903 µs |
| per fetch — `decode_us` | 125 | 331.3 µs | 170 µs | 1000 µs | 26 µs | 1920 µs |
| **per block (derived, ÷ 959 blocks)** — wire bytes | — | 7,889.0 B | — | — | — | — |
| per block (derived) — fetch wire time | — | 814.8 µs | — | — | — | — |
| per block (derived) — ingest | — | 290.6 µs | — | — | — | — |
| per block (derived) — decode | — | 43.2 µs | — | — | — | — |

Beside the derived per-block figure, the **server's exact per-block delta bytes** (measured, §3.4): mean
**10,812 B**, total 10,368,754 B over the 959 blocks. The derived client per-block figure (7,889 B) is lower
because a coalesced fetch encodes the net effect of its 2–31 blocks once, so a cell touched in several of them
is transferred once rather than once per block. Cite the server's 10,812 B for "what one block costs on the
wire"; cite the client's per-fetch row for "what a following client actually pays".

Restricted to the 30 single-block fetches (measured), where fetch and block coincide: `ingest_us` mean 560.03,
p50 407.00, p95 1293.00, min 60.00, max 1318.00; `decode_us` mean 78.20, p50 68.00, p95 146.00, min 26.00,
max 155.00. The other 95 client-block rows coalesced more than one block and are excluded from that subset.

Server block range (measured): 25,893,198–25,894,156 (959 blocks). Client block range (measured):
25,893,198–25,894,156 (125 fetch rows). Overlap: 125 rows observed by both.

## 4. C — one time

### 4.1 C11 — scale and sizes (measured, computed)

| field | measured (`time-setup`) | computed (`Geometry::sizes`) |
|---|---:|---:|
| accounts | 204,713,227 | — |
| server DB bytes | 23.62 GB (23,622,320,128 B) | 23.62 GB (23,622,320,128 B) |
| hint bytes | 553.82 MB (553,819,200 B) | 553.82 MB (553,819,200 B) |
| `A` bytes | — (not reported by `time-setup`) | 554.72 MB (554,716,800 B) |

Measured and computed agree exactly on both sizes that can be compared.

The live store, from `/metrics` (measured), at the two ends of the campaign:

| metric | at campaign-server start (2026-09-02T22:54:46Z, `/metrics`; not in the committed data — decision log R18) | at collection (04:05:31Z) |
|---|---:|---:|
| `risepir_store_items` | 204,714,034 | 204,743,822 |
| `risepir_store_cells_bytes` | 23,622,320,128 | 23,622,320,128 |
| `risepir_hint_bytes` | 553,819,200 | 553,819,200 |
| `risepir_process_rss_bytes` | 26,462,306,304 (26.46 GB) | 26,717,814,784 (26.72 GB) |

The campaign-server-start column is 1 h 52 min before the window actually opened (00:46:45Z), so it is not a
window-start figure at all. Reconstructed window-start `risepir_store_items` (derived): **≈ 204,724,899** =
204,743,822 at collection − 18,923 net inserts − deletes over blocks 25,893,198–25,894,188 (the window's rows
plus the 32 post-window rows before collection; the server per-block CSVs).

The account count moves with the chain (+29,788 over 5.2 h, 22:55 → 04:05:31 UTC, all 1,565 server rows;
+18,900 ≈ over the 3.1 h window, derived from the same server per-block CSVs); the cell and hint footprints do
not move at all, because the geometry is fixed and the store is patched in place. C11's 204,713,227 is the
count in the state file the `time-setup` run read (block 25,892,623, saved at the campaign switch, 574 blocks —
≈1 h 52 min — before block₀ 25,893,197), which is why it sits just below the live `store_items` at window
start.

### 4.2 C12 — `/setup` download and client RSS (measured)

| field | value |
|---|---:|
| `setup_bytes` | 553.82 MB (553,819,345 B) |
| `content_length` | 553.82 MB (553,819,345 B) |
| wall seconds | 0.952 |
| `pinned_block` | 25,893,197 |

Client RSS, sampled during the run (measured, `client_rss_bytes`):

| n | mean | min | max |
|---:|---:|---:|---:|
| 30 | 1.14 GB (1,141,386,718 B) | 1.12 GB (1,121,783,808 B) | 1.16 GB (1,158,127,616 B) |

The downloaded bundle is the 553,819,200 B hint plus 145 B of framing. The resident 1.14 GB is hint + `A`
re-expanded locally from its seed — the inherent SimplePIR-class client footprint at this scale.

### 4.3 C13 — full PIR setup, and the exact-hint invariant (measured)

`time-setup` ran on `risepir-state.campaign-start.bin` — the state copy saved at the campaign switch
(block 25,892,623; 204,713,227 accounts) — **on the live host**, with the campaign server idle at head between
batch 0 and batch 1, 00:49:43 → 01:42:29 UTC. It does two separate things, reported separately.

| field | value |
|---|---:|
| `setup_seconds` (the full PIR setup the bootstrap runs, 16 rayon threads) | **29.178 s** |
| `exact_check_seconds` (the invariant check, single-threaded) | 3125.5 s (52 min) |
| `persisted_hints_exact_match` | **true** |
| `persisted_hints_decode_ok` / `rebuilt_hints_decode_ok` | ok / ok |
| `state_block` | 25,892,623 |
| `lwe_dim` | 1275 |
| `rayon_threads` | 16 |
| peak RSS (`/usr/bin/time -v`) | 25,804,460 KB (25.8 GB) |
| wall clock (whole process) | 52:45.19 |

Two things to keep apart. **29.178 s is the cost figure**: one full setup over 204.7 M accounts, the same
`server_setup` the bootstrap itself calls — set it against §3.3's 3.99 ms mean per-block hint patch, which is
the whole argument of this system. **3125.5 s is not a cost figure**; it is a *verification*, deliberately
single-threaded, proving that the persisted hints — incrementally patched block by block since the 2026-08-19
bootstrap, never recomputed — reproduce Aᵀ·D bit-for-bit from the persisted seed and the store's own cells. It
would never run in production. The 256 blocks that applied while it ran are the `time-setup` subset of §3.2,
and show no effect.

### 4.4 A residential vantage, for contrast (observation — NOT campaign data)

Before the cloud client, the same probe binary and the same server were exercised from an Apple M1 laptop
(16 GB, macOS 26.6.2) on residential fibre in Da Nang, Vietnam (AS7552), 23:08–23:13 UTC 2026-09-02, session
pinned at block 25,892,719. Batch 0 completed: n = 100, all 100 byte-identical against the independent
provider. It is reported here as an *observation about a network path*, not as a campaign measurement, and it
is not mixed into any statistic above.

| quantity | mean | p50 | p95 |
|---|---:|---:|---:|
| A1 (`t_total`) | 2311 ms | 2151 ms | 2900 ms |
| A2 (`build`) | 80.9 ms | — | — |
| head RTT | 261.9 ms | — | — |
| answer wire | 1925.6 ms | — | — |
| A4 (server compute) | 609.8 ms | — | — |
| A5 (`finish`) | 19.4 ms | — | — |
| A3 (derived) | 1600 ms | 1438 ms | 2186 ms |

`/setup` downloaded 553,819,345 B in 65.0 s (against 0.952 s from the cloud vantage). **A4 is 609.8 ms here and
609.6 ms from AWS** — the server does not care where the client sits; everything else in the table is the path.
The run was abandoned when the link degraded: the probe log records 31 `follow step failed (Pir); continuing`
lines between 00:10:16 and 00:20:08 UTC, and batch 1's answer wire times rose to a 2350 ms mean (max 3713 ms)
against batch 0's 1926 ms. It was replaced by the AWS client at the user's request. The raw files
(114 trial rows: batch 0's 100 plus the 14 of batch 1 that were written before the probe was stopped) are
committed under [`docs/data/deployment-20260903/mac-vantage/`](data/deployment-20260903/mac-vantage/).

## 5. Correctness evidence

**300 private queries returned byte-exact balances against an independent provider across 52 distinct blocks
(25,893,197–25,894,124).** Byte-exact means the raw JSON-RPC hex string, not a numeric comparison: 300/300
matched numerically *and* 300/300 were byte-identical in rendering, with 0 mismatches, 0 provider-unavailable,
0 errored trials, and 0 rows needing more than one attempt. Each check asks `https://ethereum-rpc.publicnode.com`
— a different operator from the feed — for the same address at the *same explicit block height* (ADR-0007:
never `"latest"` against `"latest"`), after the trial's clock has stopped. 267 of the 300 found an account and
33 did not; of those 33, **26 are the deliberate random-address probes** (`absent_probe`), and none of the 26
was found — an absent answer is as much a correctness claim as a present one, and it was checked against the
provider the same way.

The server's own reconcile backstop, at collection (`healthz-end.txt`, measured): `ok 25894188`,
`reconcile_last_success_block` 25,894,170, `reconcile_checkpoints_total` 52, `reconcile_comparisons_total` 416
(8 accounts per checkpoint, all exact), `reconcile_consecutive_dark` 0, `reconcile_halted` 0. Inside the
campaign window the log records 32 `reconcile at block N: 8 account(s) exact vs independent provider` lines and
**zero** `CRITICAL` lines (the last one is 2026-09-02T22:49:57Z, during the pre-campaign catch-up, when the
backstop was legitimately dark).

And the invariant, from §4.3: the persisted hints — patched incrementally through every block since the
2026-08-19 bootstrap and never recomputed — equal Aᵀ·D recomputed from the persisted seed, **byte for byte**.
That is the strongest statement in this document: the incremental path has not drifted from the ground truth it
is supposed to track.

## 6. Limitations

- **Window and diurnal coverage.** 3.13 h starting 00:46:45Z covers one part of one day. §3.1's mutation
  distribution is real for those hours and is not a representative sample of Ethereum's activity cycle.
- **One vantage.** One cloud client (AWS us-east-1 → GCP us-east4, both Northern Virginia), plus the
  residential aside of §4.4. A3 is that path's cost, not the server's; A1 inherits it.
- **Scale.** Measured at 204.7 M accounts (the complete nonzero-balance set). Nothing here is scaled to another N.
- **What the 300/300 does and does not say.** The bootstrap data itself has a known residual inaccuracy — the
  server's own audit reports `snapshot_audit=checked=200 disagreed=2 block=25778250 rate=1.00%
  ci=[0.27%,3.57%]` (ADR-0040/0041; the hard-refresh pass never ran to completion). That is a property of the
  2026-08-19 BigQuery export, **not of PIR**: the private-retrieval path returns exactly what the store holds,
  and §4.3 proves the store's hints match the store's cells. So the 300/300 byte-exact result is over the
  *probed* addresses — recently-active accounts from `GET /recent` plus random absent addresses — and is not a
  statement about every account in the set.
- **Reconcile depth.** Deferred-reservoir reconcile checks at deep blocks get HTTP 403 from the keyless
  provider (archive depth) — expected, and deferred rather than counted as a failure. It did not arise in this
  window: the server was at head throughout, and `healthz-end.txt` accordingly reports
  `reconcile_deferred_total=0` and `reconcile_reservoir_checks_total=0`. So this campaign says nothing about
  the reservoir path either way.
- **Per-block client cost is derived.** §3.5: the client's measurement unit is a fetch (2–31 blocks coalesced,
  because the server follows `finalized`); the per-block figure is that divided by blocks. The server's exact
  per-block bytes are printed beside it for the undiluted number.
- **C13's exact check is a verification, not a cost.** 52 min, single-threaded, deliberately. The cost figure
  is `setup_seconds` = 29.178 s.
- **C12 client RSS is the probe process** (`rustls`, one session) on the r7a.xlarge. The browser client's
  memory — whose real ceiling is the init peak, not steady state (ADR-0032 revision) — is not measured here.
- **DNS.** The public `demo.risepir.org` record was not moved during the campaign; the client resolved the
  hostname to the host's address itself (`--resolve`), with certificate validation left on. This changes which
  IP the client dials and nothing else.
- **`time-setup` overlapped the window.** 256 of the 959 blocks applied while C13 was running on the same host.
  Its subset is broken out in §3.2 and shows no effect, but it is disclosed rather than hidden.
- **Not measured here.** Bootstrap-from-snapshot time; catch-up replay rate (both are `docs/deploy.md` §5.11);
  the browser client; any vantage outside Northern Virginia except §4.4; behaviour under concurrent clients —
  the probe is deliberately sequential, one query in flight, so nothing here describes a loaded server.

## 7. Reproduce

Every command below is the one that actually ran. Server-side commands run on the deployment host; probe
commands on the client instance; `xtask report` anywhere with the repo and the raw files.

**1 — the campaign server** (after an anchored `SIGINT` stop of the previous server, waiting for
`state saved; exiting`, and `cp ~/risepir-state.bin ~/risepir-state.campaign-start.bin` for step 2):

```bash
tmux new-session -d -s risepir "cd ~/build-4 && exec ./target/release/risepir-rpc mainnet \
  --state ~/risepir-state.bin --web web \
  --answer-timing-header --block-metrics-csv ~/campaign/server-blocks.csv \
  >> ~/server-complete.log 2>&1"
```

`--answer-timing-header` and `--block-metrics-csv` are the two ADR-0048 measurement flags; everything else is
the ordinary production start line. `--prefetch` is left at its default of 1 — the server is at head, not
catching up.

**2 — C13, `time-setup`** (on the state copy, server idle at head):

```bash
/usr/bin/time -v ./target/release/risepir-rpc time-setup \
  --state ~/risepir-state.campaign-start.bin --out ~/campaign/setup.json
```

**3 — the probe** (on the AWS r7a.xlarge, from a build of the same commit):

```bash
./target/release/risepir-rpc probe \
  --pir-url https://demo.risepir.org \
  --resolve demo.risepir.org:443:35.199.37.209 \
  --queries-csv ~/campaign/trials.csv \
  --blocks-csv ~/campaign/client-blocks.csv \
  --batch-size 100 --batches 3 --batch-interval-secs 5400 \
  --follow-secs 11400 --poll-secs 12 --trial-gap-ms 500 --absent-fraction 0.1
```

`--confirm-url` is left at its default (`https://ethereum-rpc.publicnode.com`). The address never reaches the
PIR server, and never enters a CSV, a log line or an error message — only `found`, `provider_match` and
`provider_hex_match`, one bit each. The confirm call is the single, deliberate exception: it asks the
*independent* provider about the address in plaintext, which is the check itself.

**4 — collect** (`collect.sh`): `scp` the client's `trials.csv`, `client-blocks.csv`, `probe-stdout.log` and
`window-start.txt`; on the server, snapshot `/metrics` and `/healthz` and
`grep -E "reconcile at block|state loaded|CRITICAL" ~/server-complete.log | tail -400`, then `scp` the server
CSV and those snapshots.

**5 — trim the server CSV to the window.** The server CSV starts when the server does, not when the probe does.
The trim rule is: keep `first_block ≤ block ≤ last_block`, where `first_block` is the first block applied after
the session pinned at block₀ and `last_block` is the last applied before the probe exited — here
[25,893,198, 25,894,156]. That cut 574 pre-window and 32 post-window rows from 1,565, leaving 959. **Both files
are committed**: the trimmed one is what every statistic above is computed from, the untrimmed one so the trim
itself can be audited rather than trusted —

```bash
cd docs/data/deployment-20260903
awk -F, 'NR==1 || ($1>=25893198 && $1<=25894156)' server-blocks.csv \
  | diff - <(tr -d '\r' < server-blocks-window.csv) && echo identical
```

**6 — render the tables:**

```bash
cargo run -p xtask --release -- report \
  --trials         docs/data/deployment-20260903/trials.csv \
  --client-blocks  docs/data/deployment-20260903/client-blocks.csv \
  --server-blocks  docs/data/deployment-20260903/server-blocks-window.csv \
  --setup          docs/data/deployment-20260903/setup.json \
  --setup-download docs/data/deployment-20260903/setup-download.json \
  --provenance     docs/data/deployment-20260903/provenance.json
```

Every §2–§4 figure that carries an n/mean/p50/p95 comes from that command's output, unmodified — re-run over
the committed files it reproduces the same tables byte-for-byte. The per-fetch and derived per-block rows of
§3.5, the four-way B8 split of §3.2, and the K row of §3.1 are computed from the same committed CSVs; all of
them, and ~180 of the tool's own figures, were independently recomputed in Python, with no mismatches.

## 8. Infrastructure outcome

- **Old host retired.** `risepir` (`e2-highmem-8`, us-central1-a) had been stopped since 2026-08-26 and was
  retired on 2026-09-02. Snapshot `risepir-pre-migration-20260903` (32.2 GB) was taken first; the instance and
  its 250 GB disk were deleted at ~23:05 UTC, only after the new deployment was verified end to end (serving,
  `/mode` = 1, at head, reconcile green at head, 20/20 probes byte-exact against the independent provider).
- **Measurement host.** `risepir-c3d` (`c3d-highmem-16`, us-east4-a) — the host every number in this document
  was measured on. Cross-region because `c3d-highmem-16`/`-8` were stocked out in every us-central1 zone that
  day (§9, D1).
- **Costs**, verified against the Cloud Billing catalog (service `6F81-5844-456A`, USD, on-demand):
  `c3d-highmem-16` in Virginia = 16 × $0.029563/h + 128 × $0.003959/GiB-h = $0.9798/h ≈ **$23.5/day** running.
  Balanced PD is $0.10/GiB-month in us-central1 and $0.11 in Virginia, so a 250 GB disk is $25.0/month
  (central) or $27.5/month (Virginia) idle; PD snapshots are $0.05/GiB-month (the 32.2 GB pre-migration
  snapshot ≈ $1.6/month); a static IP is $0.010–0.011/h ≈ $7.3–8/month each.
- **Client instance.** The AWS r7a.xlarge was torn down after the window: instance terminated, security group
  `sg-0c4fdbb9415a29d50` and key pair `risepir-probe-20260903` deleted, the local `.pem` removed.
- **Final placement: moved back to `us-central1-a`.** The deployment now serves from `risepir-c3d`
  (`c3d-highmem-16`) on boot disk `risepir-c3d-central` (250 GB pd-balanced, restored from snapshot
  `risepir-campaign-final-20260903`, 38.2 GB stored, taken from the measurement host's final save at block
  25,894,188, 04:08:33 UTC), under the **original** reserved address `risepir-ip` = `136.115.93.177` — so
  `demo.risepir.org` resolves to it with **no DNS change**, removing the manual Cloudflare step §5.11 left
  pending. Verified 2026-09-03 04:29–04:36 UTC: `GET /mode` = 200 from `136.115.93.177` at 04:29:50 UTC, head
  25,894,316, lag 0, reconcile green (`reconcile_last_success_block=25894320`,
  `reconcile_consecutive_dark=0`). The us-east4-a measurement host was stopped gracefully, snapshotted, and
  deleted with its disk at ~04:33 UTC; the `risepir-ip-east4` address was released. Inventory after cleanup:
  one instance (`risepir-c3d`, `us-central1-a`, running), one disk (`risepir-c3d-central`), two snapshots
  (`risepir-pre-migration-20260903` 32.2 GB, `risepir-campaign-final-20260903` 38.2 GB), one address
  (`risepir-ip`, in use). Compute cost is unchanged (`c3d-highmem-16` = 16 × $0.029563/h + 128 × $0.003959/h =
  $0.9798/h ≈ $23.5/day, the same catalog price in both regions); the 250 GB disk drops to $25.0/month at the
  central-region rate (was $27.5/month in Virginia), snapshots ≈ $3.5/month for both, and the reserved address
  ≈ $0.010/h. Full record, including the move-back sequence itself: `docs/deploy.md` §5.12.

## 9. Decision log

Written before execution (D1–D11) and appended during it (R1–R31), never rewritten. Reproduced here lightly
edited for readability; all times UTC.

### Decisions

- **D1 — Host: GCP `c3d-highmem-16` (AMD EPYC 9B14, Zen 4, 8 cores/16 threads, 250 GB pd-balanced),
  us-east4-a.** Zen 4 is the same microarchitecture as the paper's r7a.xlarge (EPYC 9R14), and it doubles the
  old host's cores and memory. `c3d-highmem-16`/`-8` were stocked out in all four us-central1 zones (a, b, c, f)
  at 17:20–17:30 on 2026-09-02, and `c3d-highmem-30` is over the C3D quota; us-east4-a had capacity. The
  cross-region move costs a new regional static IP (`risepir-ip-east4` = 35.199.37.209) and a DNS change for
  `demo.risepir.org`, which `docs/deploy.md` records as a Cloudflare-dashboard operation with no API path in
  this repo — so the flip is left to the operator and stated as such, and measurements pin the hostname
  client-side instead (the certificate on the cloned disk is valid until ~2026-11-15).
- **D2 — Migration vehicle: a disk snapshot,** `risepir-pre-migration-20260903`, of the old boot disk into a new
  disk in us-east4-a. It carries the 24.18 GB `RPST3` state file, Caddy and its certificate, the toolchain and
  the repo, so no BigQuery bootstrap is needed — and the snapshot is also the backup the retirement sequence
  requires anyway.
- **D3 — Binary rebuilt natively on the new host,** because `target-cpu=native` is not portable across
  microarchitectures. The catch-up replay (state at block 25,838,386, saved 2026-08-26; ~50k blocks) ran first
  on upstream `main` `67ac7ce`; the campaign binary replaced it before the smoke run. The replay is not a
  reported quantity.
- **D4 — Staleness operating point: one session, never garbage-collected.** Pinned at the campaign start
  (block₀ = the `/setup` block), following head for the whole window — exactly what the product client does
  (`risepir-rpc client` and the wasm client never call `collect_garbage`). A5 is therefore reported over a
  stated staleness range *and* binned by staleness; every trial row carries `stale_blocks` and `delta_cells`.
- **D5 — The A budget.** The probe drives the real session code path (`sync_to` → `build_query` → POST
  `/answer` → `finish` → `sync_to`) with `Instant` timers at each boundary. A3 = the wire time of every network
  call in the trial minus the server's reported per-request handler time (from a flag-gated response header);
  server handler-minus-compute and client bookkeeping are explicit residual rows. A1 is the trial's total wall
  time, and the residual is the subtraction — never distributed.
- **D6 — Interference control.** The probe is sequential (one connection, one query in flight) and runs in three
  batches at the window's start, middle and end, which also samples staleness ≈0 / ≈450 / ≈900 blocks. The
  server's per-block CSV records answers served since the previous block and the write-lock wait, so B8 can be
  reported with and without probe-adjacent blocks.
- **D7 — C13.** A `time-setup` mode loads the state file (I/O, untimed), re-runs the real PIR setup over the
  served store (timed), and checks the recomputed hints against the persisted, incrementally patched ones
  bit-for-bit. Run with the live server stopped or idle, on the campaign binary, on the campaign host.
- **D8 — Window ≥ 3 h** for per-block rows (~900 blocks). Diurnal coverage is a stated limitation.
- **D9 — PR shape.** Instrumentation PR first (draft → ready → review → merge), with the campaign run at the
  merged commit; data, report and doc sweep in a second PR.
- **D10 — Old host.** Already stopped when work began (`TERMINATED`, 136.115.93.177 detached); snapshot taken;
  deletion deferred until the new deployment was verified end to end.
- **D11 — Bounded in-order prefetch as its own issue and PR** (see R2): opt-in `--prefetch k`, default 1 =
  unchanged behaviour, used for the catch-up only. Implemented on the stronger model because a skipped block is
  a wrong balance — safety-critical — and it sat on the critical path.

### Revisions and findings during execution

- **R1 (17:45).** State loaded on the new host in 113.4 s: block 25,838,386, **203,879,841 accounts** — the
  docs' 201,059,658 was stale, because the box was re-bootstrapped on 2026-08-19 and no doc recorded it. C11
  would come from the campaign binary rather than from any doc.
- **R2 (17:45).** Catch-up replay measured at 1.11 blocks/s (300 blocks in 270 s) with 1,293 dRPC fetch
  failures in 7 min; only dRPC serves keyless `prestateTracer` (blastapi, llamarpc, blockpi, merkle, mevblocker
  and nodies were all probed; none do). ~52k blocks ⇒ ~13 h. Hence D11.
- **R3 (17:45).** Trap: the cloned disk's `target/` held kernel crates compiled for the *old* CPU, and Cargo
  reused them (a 9 s "incremental build") because it cannot see that `target-cpu=native` changed. The campaign
  binary was built after `cargo clean`.
- **R4.** Patch time during catch-up on the new host: ~4.2 ms mean over 300 blocks at K ≈ 342 (upstream `main`
  `67ac7ce`, non-native kernels) against 11.6 ms on the old host the day it was stopped. Observation only — not
  a campaign number.
- **R5 (17:55).** Prices verified against the Cloud Billing catalog (see §8).
- **R6 (18:40).** Probe verifier: A1/A2/A5/A6, budget closure (0 violations on 50 rows), session semantics,
  connection reuse and file privacy all PASS. Fixes requested and taken: in-trial syncs must also write block
  rows; a re-bootstrap `/setup` gets its own columns (`setup_wire_us`/`setup_bytes`, inside the budget
  identity); add `provider_hex_match` (raw provider string against the exact JSON-RPC rendering); qualify the
  "never leaves this machine" banner, since the confirm call sends the address to the independent provider by
  design.
- **R7 (18:45).** The server implementer reported that `server_setup` samples a fresh random seed for `A` on
  every call, so "recomputed hints == persisted hints" could not be a byte comparison without a with-seed API;
  a sampled decode-verify shipped instead. Escalated to the verifier as an explicit question.
- **R8 (19:05).** Server verifier: B7/B8/B9/A4/C11/CSV/metrics PASS (B9 confirmed byte-exact against the live
  `/delta` route; the A4 header is the histogram's own `Duration`; the stage timers are exactly store/fold/patch).
  On C13 the R7 premise was **wrong**: `SimpleParams.seed` is public, `expand_hint_material` is deterministic,
  and a `RowLevel` `server_patch_hint` from a zero hint over the whole store reproduces Aᵀ·D bit-for-bit
  (verified at three shapes). So `time-setup` reports **both** `setup_seconds` (the fresh full setup = the
  bootstrap's own computation, C13) and `persisted_hints_exact_match` (a chunked exact recomputation with the
  persisted `A`, separately timed); the sampled decode check stays as a smoke test under an honest name.
- **R9.** ADR numbering: ADR-0047 = follow-loop prefetch; ADR-0048 = the instrumentation and campaign design.
  `docs/deploy.md` §5.10 = the prefetch A/B evidence, §5.11 = the migration and campaign evidence.
- **R10 (19:10).** Switched the catch-up to the prefetch binary. First start **failed**: `web/client.wasm` is a
  build artifact absent from a fresh clone (`--web web: web/client.wasm: No such file`). Restarted with the
  wasm copied across, at `--prefetch 4` per ADR-0047.
- **R11 (19:15).** PR #7 CI failed on `rustdoc -D warnings` (a public doc linking a private module); fixed and
  pushed.
- **R12 (19:29:46).** PR #7 (prefetch) approved and merged. Depth 4 measured 3.75 blocks/s on the live catch-up
  (270 blocks in 72 s) with a flat failure count.
- **R13 (19:50).** Combined instrumentation branch assembled; mock rehearsal: 40/40 trials with
  `server_compute_ns`/`server_handler_ns` present and compute ≤ handler ≤ wire, the budget closing on every row,
  `time-setup` exact match true, the report rendering. Two report defects found and fixed (a B9 header
  mislabel; A4 not a first-class row).
- **R14 (19:40).** Prefetch depth 8 observed for 20 min: 300-block segments took 62–70 s at first (≈4.5
  blocks/s) then 136–171 s (≈2 blocks/s) as dRPC 408s rose (+156 fetch failures in 20 min, flat at depth 4) and
  merkle.io answered 429 to the header fetches — the same or worse throughput with more pressure on the
  keyless fallbacks while the reconcile backstop was already dark. Switched back to depth 4. The `CRITICAL`
  lines in the log during a deep catch-up are ADR-0027's "reconcile dark for N checkpoints" escalation, which is
  expected there; the loop never halts on it, but reconcile had to be seen green at head before the deployment
  could be called verified.
- **R15 (20:00).** PR #8 opened as a draft. C13's plan refined: at the campaign switch the catch-up server's
  shutdown save is copied aside, and `time-setup` runs on *that copy*, on the same host and binary, with the
  live server idle — so neither perturbs the other, and the copy is the store as served ~15 min before block₀.
- **R16 (20:45).** PR #8 approved (two blocking doc fixes and two suggestions taken) and merged as **`b37e4ee`**
  — the campaign commit. `xtask report`'s statistics were cross-checked by recomputing ~180 figures
  independently in Python: 0 mismatches.
- **R17 (21:00).** Doc sweep, pass 1 (the numbers-independent half) committed; every removed line was first
  checked to be a current claim. One pre-existing contradiction flagged for pass 2 — `docs/deploy.md` §4's
  "Migration: the `xxh3_128` pin bump — REQUIRED, and NOT YET RUN", which *was* run on 2026-07-31 (§5.8) — to be
  **annotated, not rewritten**. Old-host retirement plan fixed: verify end to end, then delete the instance and
  disk, keeping the snapshot and the reserved address until the DNS record is flipped, so a stale record times
  out rather than resolving to a stranger.
- **R18 (22:55). Campaign switch.** Catch-up reached head at 22:51:58 (head = finalized = 25,892,623); reconcile
  went green at head (block 25,892,610: 8/8 exact); graceful stop wrote `state saved (shutdown): block
  25892623, 24.18 GB in 123.5s`; the file was copied aside for C13; the campaign binary started at 22:54:46 from
  `~/build-4` at `b37e4ee` with the two measurement flags. Live: `/mode` = 1, lag 0, `store_items` 204,714,034,
  cells 23,622,320,128 B, hint 553,819,200 B, RSS 26.46 GB. Known deployment limitation recorded for §6: the
  snapshot audit's 1.00% disagreement rate.
- **R19 (23:03).** Smoke on the live path: 20/20 trials ok, 20/20 byte-identical. Observation that shaped §3.5:
  the server follows `finalized`, which advances ~32 blocks per epoch, so client delta fetches coalesce — B10
  must be reported **per fetch (measured) and per block (derived)**, with the server's exact per-block delta
  bytes beside it. Old host retired after end-to-end verification.
- **R20 (23:10). Finding.** A second `risepir-c3d` instance existed in us-central1-a, booted from the clone disk
  and holding the old reserved IP. Cause: a retry loop was killed at ~17:30 while a `gcloud compute instances
  create` was in flight; that call succeeded after the script died, and instances were never re-listed. It ran
  idle ~5.5 h (~$5.4) with no server process. **Lesson: after killing a retry loop, list the resources it could
  have created.** It also opened the post-campaign option of moving back to us-central1-a under the original IP,
  which would remove the manual DNS step entirely.
- **R21 (23:09).** The first campaign launch never ran: `setsid` does not exist on macOS, so the launch subshell
  died and the pid read back was the subshell's. Detected 4 min later by the missing CSVs and relaunched. The
  server was idle in between — no effect.
- **R22 (23:15).** Docs-sweep verifier: PASS on all seven checks. Two items deferred to pass 2: the
  "$25/mo stopped" line needed its catalog citation (now §8), and `risepir-ip` was not "detached" — it was
  detached from the old VM at 17:20 and re-attached at 17:28 by the accidental instance of R20.
- **R23 (00:40, 2026-09-03). Vantage change, user-initiated and evidence-backed.** The residential link showed
  ~10-min connect/disconnect events; the probe log recorded repeated `follow step failed (Pir); continuing`
  lines and batch-1 answer wire times of 2.1–3.7 s against 1.5–2.0 s in batch 0. Decision: stop the laptop probe
  (00:39:35) and re-run the client from a rented AWS r7a.xlarge in us-east-1 — the paper's own instance type, a
  stable link, cloud-to-cloud public internet, reproducible by anyone. The server side was unchanged (same
  binary, same host, still at head), so the campaign lineage holds; the window restarts at the new probe's
  `/setup`. The laptop's batch 0 is kept as a labelled aside (§4.4), **not** as campaign data.
- **R24 (00:50).** AWS client up and verified (5/5 smoke trials, 5/5 hex-identical), with a 9 h self-terminate
  failsafe. **Campaign window (final): opened 2026-09-03T00:46:45Z**, pinned block 25,893,197, batch 0 at
  00:46:47Z, three batches of 100 at 0 / +1.5 h / +3 h, follow 11,400 s. Server untouched throughout.
- **R25 (00:52).** Post-window work parallelised: C13 started at 00:49:43 on the state copy while the server was
  idle at head *between* batches (batch 0 ended 00:49; batch 1 fired 02:16), its start/end timestamps flagging
  the blocks applied during it as a separate B8 subset. Concurrent probes were rejected — they would share the
  server's rayon pool and contaminate A4.
- **R26 (01:15).** C13 in its single-threaded exact-check phase at 25 min. Rule set: if still running at 02:08
  (batch 1 fires 02:16), kill it, so the 16-thread rebuild cannot overlap a probe batch and perturb both A4 and
  C13; re-run after the window instead.
- **R27 (01:28).** `docs/numbers.md` regenerated against `v0.2.0-perf`; the deployment full-rebuild constant and
  its implied per-block arithmetic removed; its §7 now defers to this file and labels the old Run B
  extrapolation superseded. Geometry at 204.7 M accounts unchanged; load 0.7626 (was 0.7490 at 201,059,658).
- **R28 (01:45). C13 done** — see §4.3. Its 29 s parallel rebuild overlapped no probe batch.
- **R29 (01:50).** AWS batch 0 (n = 100) consistent with the final numbers; budget violations 0; 100/100
  hex-identical.
- **R30 (04:10). Campaign complete.** Window 00:46:45 → 03:54:37 (3.13 h); server rows 25,893,198–25,894,156
  (959 blocks; 574 pre-window and 32 post-window rows cut); 300/300 trials ok and byte-identical; 125 fetches
  covering 959 blocks. AWS client, security group and key pair torn down.
- **R31 (04:12).** Move-back step 1 done: the campaign server stopped cleanly (`state saved (shutdown): block
  25894188, 24.18 GB in 123.5s` at 04:08:33). The script then aborted on its own liveness check, because
  `pgrep -fa "target/release/risepir-rp[c]"` matched the ssh wrapper's own command line — the `docs/deploy.md`
  §5.9 trap, reproduced — and was resumed with a wrapper-safe check.

## 10. Stale-record sweep

The campaign invalidated a number of recorded claims elsewhere in the repo (account counts, host and machine
type, the IKPIR pin, the deployment's cost). Four passes have landed in this PR.

**Pass 1 — numbers-independent** (could be done before the campaign produced figures):

| target | what changed |
|---|---|
| `docs/roadmap.md` | IKPIR pin claim updated to `v0.2.0-perf` |
| `docs/adr/README.md` | forward-pointer tags added to the ADR-0045 and ADR-0023 headings |
| `docs/HANDOFF.md` | process footer and deployment-host reference updated |
| `CLAUDE.md` | "The live GCP deployment" section rewritten for the c3d migration |
| `docs/deploy.md` §5.11 | the `c3d-highmem-16` migration recorded |
| `docs/deploy.md` §3.7 | annotated as pre-migration evidence rather than rewritten |

**Pass 2a — the numbers themselves**, once the campaign start had verified them live:

| target | what changed |
|---|---|
| `docs/numbers.md` | regenerated against `v0.2.0-perf`; `DEPLOYMENT_ACCOUNTS` = 204,714,034 with its lineage; the deployment full-rebuild constant and its implied arithmetic removed; §7 now defers to this file |
| `crates/xtask/src/bench.rs` | the superseded deployment figures dropped from the harness's own §7 text |
| `CLAUDE.md` | account-count claims refreshed to the 2026-09-03 campaign start |
| `site/index.html` | the served account count updated to 204,714,034 |
| `docs/deploy.md` §1.5, §2.3 | account-count claims refreshed |
| `docs/deploy.md` §4 | the `xxh3_128` pin-bump migration **annotated as executed** (2026-07-31, §5.8) rather than rewritten — the heading had said "NOT YET RUN" |
| `crates/risepir-server/src/server.rs`, `crates/risepir-http/src/metrics.rs` | account-count figures in doc comments refreshed |
| `docs/threat-model.md`, `docs/HANDOFF.md` | remaining stale account-count claims refreshed |

**Pass 2b — the timing figures** that this campaign supersedes, from a parallel worktree:

| target | what changed |
|---|---|
| `CLAUDE.md` | the stale per-block-patch and `/setup` timing claims superseded with the campaign's own B8/C13 figures |
| `README.md` | cites `docs/deployment-numbers.md`; test count refreshed to 513 |
| `docs/HANDOFF.md` | test count refreshed to 513 |
| `docs/roadmap.md` | test count refreshed to 513; the `/setup` size re-confirmed against the campaign |
| `docs/plan.md` | test count refreshed; §7 now points at `docs/deployment-numbers.md` for the measured per-block figures |
| `site/index.html` | the served per-block patch-time stat replaced with the campaign's B8 figure |
| `web/app.js` | the CLI client's measured resident memory noted beside the computed estimate |

**Pass 3 — this finalization pass**, fixing presentation defects an independent verifier found after recomputing
every number in this document from the committed data with 0 discrepancies (the values were already right; the
labels, citations and a few stale prose figures were not), plus the deployment's final infrastructure state:

| target | what changed |
|---|---|
| `docs/deployment-numbers.md` §0 | the cache-topology row cites the new raw `lscpu` evidence |
| `docs/deployment-numbers.md` §3.1 | the K = changes + credits row relabelled `(derived)` |
| `docs/deployment-numbers.md` §4.1 | the "at window start" column relabelled to what it actually is (campaign-server start, not window start), with a reconstructed window-start count added; the account-count-growth sentence corrected (5.2 h / 3.1 h, not 3.3 h) |
| `docs/deployment-numbers.md` §4.2, §4.4 | the `/setup` wall time corrected from the hand-rounded 1.0 s/1.000 s to the precise 0.952 s (952,466 µs, `probe-stdout.log`) |
| `docs/deployment-numbers.md` §8 | the pending move-back replaced with the final `us-central1-a` placement |
| `docs/data/deployment-20260903/lscpu-server.txt` | added — raw `lscpu` output substantiating the §0 cache-topology claim |
| `docs/data/deployment-20260903/README.md` | notes that `setup-download.json` was hand-assembled from `probe-stdout.log` (bytes exact, `wall_seconds` rounded) |
| `docs/deploy.md` §5.12 | the move back to `us-central1-a` recorded |
| `CLAUDE.md` | "The live GCP deployment" rewritten for the final `us-central1-a` placement; the pending-DNS-flip paragraphs removed now that the address survived the round trip |
| `crates/risepir-wasm/src/abi.rs`, `crates/risepir-wasm/src/session.rs`, `crates/risepir-http/src/node.rs`, `crates/risepir-http/tests/setup_cache.rs`, `crates/risepir-rpc/src/private_eth.rs` | stale "~831 MB" doc comments (pre-existing since ADR-0034) corrected to 553.82 MB, with the pre-ADR-0034 figure kept as an explicit historical aside |

Nothing in the sweep changes behaviour; every edit is a doc, a doc comment, or raw evidence, and every removed
line was checked to be a current claim before removal rather than a historical record (historical records were
annotated, per R17).
