# Raw data — deployment measurement campaign, 2026-09-03

The complete raw output of the campaign reported in [`docs/deployment-numbers.md`](../../deployment-numbers.md).
Every statistic in that report is recomputable from these files alone; the exact `xtask report` command is its
§7. Nothing here has been edited, reordered or filtered except the one documented trim below. All times UTC.

Campaign commit `b37e4ee`; server `risepir-c3d` (GCP `c3d-highmem-16`, us-east4-a); client an AWS `r7a.xlarge`
in us-east-1b; window 2026-09-03T00:46:45Z → 03:54:37Z.

## Files

| file | what it is | rows |
|---|---|---:|
| `trials.csv` | one row per private query — the A-series. Timers, wire bytes, staleness, and the independent-provider verdict | 300 |
| `client-blocks.csv` | one row per **delta fetch** the client made (not per block: fetches coalesce, `blocks_in_fetch` says how many) — the client half of B9/B10 | 125 |
| `server-blocks-window.csv` | one row per block **applied during the campaign window** — B7/B8/B9. **This is the file the report's statistics are computed from** | 959 |
| `server-blocks.csv` | the same CSV **untrimmed**, exactly as the server wrote it from its own start | 1,565 |
| `setup.json` | `time-setup` output — C11 sizes, C13 `setup_seconds`, and the exact-hint invariant result | — |
| `setup-download.json` | the probe's `/setup` download — C12 bytes, wall time, pinned block. **Hand-assembled**, not a tool artifact: `setup_bytes`/`content_length`/`pinned_block` are copied verbatim from `probe-stdout.log`'s `setup 553819345 B in 1.0s — hint pinned at block 25893197` line, so they are byte-exact; `wall_seconds` was typed from that same line's rounded `1.0s` display rather than from the log's own precise `hint download (C12) 553819345 B in 952466 us` summary line, so it reads `1.0` where the precise figure is 0.952466 s (952,466 µs). `docs/deployment-numbers.md` §4.2 reports the precise 0.952 s, not this file's rounded one | — |
| `provenance.json` | the provenance block `xtask report` prints verbatim (commit, hosts, binary hashes, link, feed, window) | — |
| `probe-stdout.log` | the probe's own console output: banner, batch starts, and its end-of-run summary | — |
| `time-setup.log` | the C13 wrapper's log — start/end stamps, the tool's own output, the `/usr/bin/time` extract | — |
| `time-setup.time` | full `/usr/bin/time -v` output for the C13 process (peak RSS, wall clock, CPU) | — |
| `time-setup-start.txt`, `time-setup-end.txt` | the wrapper's start and end stamps, used to flag the 256 blocks that applied during C13 (§3.2's fourth B8 subset). The tool's own end stamp inside `time-setup.log` is 01:42:29Z; `time-setup-end.txt` is the wrapper's, written 30 s later | — |
| `metrics-end.txt` | `GET /metrics` at collection — store items, cell/hint bytes, RSS, the answer-latency histogram, reconcile counters | — |
| `healthz-end.txt` | `GET /healthz` at collection — head block, reconcile state, snapshot-audit result | — |
| `log-excerpt.txt` | `grep -E "reconcile at block\|state loaded\|CRITICAL"` over the server log, last 400 matches | — |
| `window-start.txt` | the campaign window's start stamp, written by the launcher | — |
| `collected-at.txt` | when these files were collected off the two hosts | — |
| `mac-vantage/` | a **separate, non-campaign** run — see below | — |

## Columns

The CSV schemas are documented next to the code that writes them, not duplicated here:

- `trials.csv` and `client-blocks.csv` — `crates/risepir-rpc/src/probe.rs` (module docs, plus `TrialRow` and
  `BlockRow`). The A-budget identity `t_total_us = build_us + head_wire_us + sync_wire_us + answer_wire_us +
  setup_wire_us + finish_us + residual_us` closes by construction on every row, with `residual_us` defined as
  the subtraction.
- `server-blocks*.csv` — `crates/risepir-rpc/src/block_metrics_csv.rs` (the `HEADER` constant and `BlockRow`).
  Every field is a plain unsigned integer or a fixed-precision decimal; `answers_since_prev_block` and
  `answer_compute_ms_since_prev_block` are empty on the first row a follow-loop run writes, whose window is
  undefined (no such row survives the trim here).
- The design behind all three, and why each measurement exists: ADR-0048 in `docs/adr/README.md`.

## The trim rule

`server-blocks.csv` starts when the *server* started (22:54 on 2026-09-02), not when the probe session did.
`server-blocks-window.csv` keeps `25,893,198 ≤ block ≤ 25,894,156` — from the first block applied after the
session pinned at block₀ = 25,893,197, to the last applied before the probe exited. That cut **574 pre-window**
and **32 post-window** rows from 1,565, leaving 959.

Both files are committed on purpose. The trimmed one is what the report computes from; the untrimmed one is
what makes the trim auditable instead of merely asserted — anyone can re-derive the window file and diff it
against the committed copy:

```bash
awk -F, 'NR==1 || ($1>=25893198 && $1<=25894156)' server-blocks.csv \
  | diff - <(tr -d '\r' < server-blocks-window.csv) && echo identical
```

The `tr` is not cosmetic: `server-blocks.csv` is written by the server (LF), while `server-blocks-window.csv`
came out of a Python `csv` writer and carries CRLF. Both parse identically — `xtask report` run over these
exact committed files reproduces the report's tables byte-for-byte.

## Privacy tripwire

No file here contains an address or a balance. The probe writes, by design, only `found`, `provider_match` and
`provider_hex_match` — one bit each — and a tripwire test in the probe's own suite
(`no_row_can_carry_an_address_or_a_balance`) pins that at the type level.

The mechanical check run before committing, and the one to re-run after touching anything in this directory:

```bash
grep -aoE '[0-9a-fA-F]{40,}' docs/data/deployment-20260903/* docs/data/deployment-20260903/mac-vantage/*
```

An Ethereum address is a 40-hex run, so the only acceptable matches are the three hashes in
`provenance.json` — the 40-hex campaign commit SHA and the two 64-hex sha256 binary digests (a sha256 contains
40-hex substrings, which is why it matches). Anything else is a leak. At commit time the check returned exactly
those three and nothing else, and zero matches anywhere under `mac-vantage/`.

## `mac-vantage/` — an observation, not campaign data

A run of the same probe binary against the same server from an Apple M1 laptop on residential fibre in Da Nang,
Vietnam (AS7552), 2026-09-02 23:08–23:13Z, session pinned at block 25,892,719. It is reported in
`docs/deployment-numbers.md` §4.4 as an observation about a *network path* and is mixed into no campaign
statistic. `trials.csv` here holds 114 rows: batch 0's 100 (all byte-identical against the independent
provider) plus the 14 of batch 1 written before the probe was stopped, after the link degraded — the
`follow step failed (Pir); continuing` lines in `probe-stdout.log` are that degradation. There is no
`setup-download.json`: the run was killed before it printed a summary, so the `/setup` figure comes from the
log line (`setup 553819345 B in 65.0s`).
