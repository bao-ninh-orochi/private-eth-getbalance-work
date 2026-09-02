//! `xtask report` — turns one measurement campaign's raw CSV/JSON files
//! into the statistics a paper cites, in markdown.
//!
//! # Purpose
//!
//! A campaign produces four kinds of raw input (trials, client-observed
//! blocks, server-observed blocks, one setup measurement — see
//! [`ReportData::parse`]'s parameters) plus a free-form provenance file.
//! This module turns them into every reported statistic, and — the whole
//! point — labels each one so a reader can tell, without guessing,
//! whether a number was read off an instrument, derived by arithmetic
//! from other reported numbers, or computed from a closed-form geometry
//! formula that was never timed at all:
//!
//! - **(measured)** — read directly from a raw input file (a CSV column,
//!   or a JSON field written by the instrument that took the
//!   measurement).
//! - **(computed)** — a deterministic, closed-form function of the
//!   geometry ([`risepir_proto::Geometry::sizes`]), never timed. Mirrors
//!   `xtask::bench`'s identical convention (see that module's docs) —
//!   this module never hardcodes a size or re-derives one by its own
//!   formula; every computed byte count in this file comes from calling
//!   [`Geometry::sizes`] on a [`Geometry`] built from the campaign's own
//!   `--setup` measurement (accounts, `num_buckets`, `bucket_size`,
//!   `plaintext_bits`), never from an assumed or hardcoded geometry.
//! - **(derived)** — arithmetic on other *reported* rows (a difference, a
//!   ratio, a sum-of-means) — never re-measured and never computed from
//!   the geometry.
//!
//! This tool only ever *reads* its input files and *prints* markdown
//! (optionally to a file named by `--write`, which is never one of the
//! inputs) — it never writes to, or otherwise modifies, any input.
//!
//! # Percentile method
//!
//! Every `p50`/`p95` in this file is the **nearest-rank** percentile over
//! the sorted *successful* samples for that column: for a quantile `q`
//! and `n` samples, the 1-based rank is `ceil(q * n)`, and the reported
//! value is the sample at that rank (0-based index `ceil(q * n) - 1`) —
//! see [`compute_stats`]. This always names an actual sample that
//! appeared in the data; it never interpolates between two samples.
//!
//! # What `n` counts
//!
//! For every trials-CSV statistic, `n` counts only rows whose `error`
//! column is empty. A row with a non-empty `error` is a failed trial —
//! its other columns (latency, bytes, `provider_match`, ...) do not
//! describe a completed private query, so it is excluded from every
//! §A statistic and tallied on its own in §D ("Interference and
//! data-quality notes"). The client-blocks and server-blocks CSVs carry
//! no `error` column, so their `n` is every parsed row (further filtered
//! to `blocks_in_fetch == 1` for the single-block statistics §B9/§B10
//! call for; see [`single_block_client_rows`]).
//!
//! # Parsing
//!
//! Every CSV is parsed by header name, not column position (a
//! `ColumnMap` built from the header line), and fails loudly — naming
//! the file and the missing column — if the header does not carry every
//! column this module expects (see e.g. `TRIALS_COLUMNS`). Column
//! lookups are table-driven (a `const &[&str]` list of expected names per
//! file) specifically so a renamed column, such as the server-blocks CSV
//! is expected to see once its own branch lands, is a one-line change
//! rather than a hunt through positional indices. The hand-rolled parser
//! (`str::split(',')`, no quoting) assumes no field contains an
//! unescaped comma — `split_row` turns any row that does not split into
//! exactly the header's column count into a loud parse error, which is
//! this module's way of "asserting" that invariant rather than silently
//! misaligning every column after the offending one.

use std::collections::BTreeSet;
use std::fmt::Write as _;

use ikpir_common::backend::simple::SimpleParams;
use risepir_proto::{Backend, Geometry};

use crate::bench::{fmt_bytes, fmt_num, value_codec, FINGERPRINT_BITS};

// ─── Labels ─────────────────────────────────────────────────────────────

/// A number read directly from a raw input file.
pub const MEASURED: &str = "measured";
/// A deterministic, closed-form function of the geometry
/// ([`Geometry::sizes`]) — never timed.
pub const COMPUTED: &str = "computed";
/// Arithmetic on other reported rows — never re-measured, never computed
/// from the geometry.
pub const DERIVED: &str = "derived";

// ─── Statistics ─────────────────────────────────────────────────────────

/// `n`, mean, and nearest-rank `p50`/`p95`/min/max over one column's
/// successful samples. See the module docs for the exact percentile
/// method and what `n` counts.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Stats {
    /// Sample count this was computed from.
    pub n: usize,
    /// Arithmetic mean.
    pub mean: f64,
    /// Nearest-rank 50th percentile (an actual sample, never interpolated).
    pub p50: f64,
    /// Nearest-rank 95th percentile (an actual sample, never interpolated).
    pub p95: f64,
    /// Minimum sample.
    pub min: f64,
    /// Maximum sample.
    pub max: f64,
}

/// The nearest-rank percentile of `sorted` (must already be ascending and
/// non-empty) at quantile `q` in `[0, 1]`: the sample at 1-based rank
/// `ceil(q * n)`, i.e. 0-based index `ceil(q * n) - 1`. Rank is clamped
/// into `[1, n]` so `q = 0` and floating-point edge cases never index out
/// of bounds.
fn nearest_rank(sorted: &[f64], q: f64) -> f64 {
    let n = sorted.len();
    debug_assert!(n > 0, "nearest_rank: empty input");
    let rank = ((q * n as f64).ceil() as i64).clamp(1, n as i64) as usize;
    sorted[rank - 1]
}

/// Summary statistics over `values` (see [`Stats`]), or `None` for an
/// empty input — every call site decides how to render "no data" rather
/// than this function inventing a zero row.
pub fn compute_stats(values: &[f64]) -> Option<Stats> {
    if values.is_empty() {
        return None;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let n = sorted.len();
    let mean = sorted.iter().sum::<f64>() / n as f64;
    Some(Stats {
        n,
        mean,
        p50: nearest_rank(&sorted, 0.50),
        p95: nearest_rank(&sorted, 0.95),
        min: sorted[0],
        max: sorted[n - 1],
    })
}

/// [`compute_stats`] over `items` mapped through `f` — the convenience
/// every per-column table in this module actually calls.
fn stats_from<T>(items: impl IntoIterator<Item = T>, f: impl Fn(T) -> f64) -> Option<Stats> {
    let values: Vec<f64> = items.into_iter().map(f).collect();
    compute_stats(&values)
}

// ─── CSV parsing infrastructure ────────────────────────────────────────

/// Maps a CSV header's column names to their position, so every field is
/// read by name — never by position — and a header that does not carry
/// an expected name fails loudly, naming both the file and the column,
/// instead of silently misaligning every read after it.
struct ColumnMap<'a> {
    names: Vec<&'a str>,
}

impl<'a> ColumnMap<'a> {
    fn from_header(header_line: &'a str) -> Self {
        Self {
            names: header_line.split(',').map(str::trim).collect(),
        }
    }

    /// Position of `name`, or a loud error naming `file_desc` and `name`.
    fn require(&self, name: &str, file_desc: &str) -> Result<usize, String> {
        self.names
            .iter()
            .position(|&n| n == name)
            .ok_or_else(|| format!("{file_desc}: missing required column {name:?}"))
    }
}

/// Splits one CSV data line on `,` and checks the field count matches
/// `expected_cols` (the header's column count). This hand-rolled parser
/// does not understand quoting, so a field containing an unescaped comma
/// would otherwise silently misalign every column after it; this check is
/// this module's "assert the inputs never contain quoted commas" — a
/// row that does not split cleanly is a loud parse error, not a silent
/// misparse.
fn split_row<'a>(
    line: &'a str,
    expected_cols: usize,
    file_desc: &str,
    row_num: usize,
) -> Result<Vec<&'a str>, String> {
    let fields: Vec<&str> = line.split(',').collect();
    if fields.len() != expected_cols {
        return Err(format!(
            "{file_desc}: row {row_num}: expected {expected_cols} fields, found {} (a field \
             may contain an unescaped comma, which this hand-rolled parser does not support): \
             {line:?}",
            fields.len()
        ));
    }
    Ok(fields)
}

/// Non-empty, non-comment lines of `content`, paired with their 1-based
/// line number within `content` — every CSV in this module skips blank
/// lines (a trailing newline is common and harmless) but nothing else.
fn csv_lines(content: &str) -> impl Iterator<Item = (usize, &str)> {
    content
        .lines()
        .enumerate()
        .map(|(i, l)| (i + 1, l))
        .filter(|(_, l)| !l.trim().is_empty())
}

fn parse_u64_field(raw: &str, file_desc: &str, row_num: usize, col: &str) -> Result<u64, String> {
    raw.trim()
        .parse()
        .map_err(|_| format!("{file_desc}: row {row_num}: column {col:?}: not a u64: {raw:?}"))
}

fn parse_i64_field(raw: &str, file_desc: &str, row_num: usize, col: &str) -> Result<i64, String> {
    raw.trim()
        .parse()
        .map_err(|_| format!("{file_desc}: row {row_num}: column {col:?}: not an i64: {raw:?}"))
}

fn parse_u32_field(raw: &str, file_desc: &str, row_num: usize, col: &str) -> Result<u32, String> {
    raw.trim()
        .parse()
        .map_err(|_| format!("{file_desc}: row {row_num}: column {col:?}: not a u32: {raw:?}"))
}

fn parse_f64_field(raw: &str, file_desc: &str, row_num: usize, col: &str) -> Result<f64, String> {
    raw.trim()
        .parse()
        .map_err(|_| format!("{file_desc}: row {row_num}: column {col:?}: not a f64: {raw:?}"))
}

/// `"1"` / `"0"` — the documented `provider_match` convention
/// (`{1,0,empty}`), applied uniformly to every required boolean column in
/// these CSVs (`found`, `absent_probe`).
fn parse_bool01_field(
    raw: &str,
    file_desc: &str,
    row_num: usize,
    col: &str,
) -> Result<bool, String> {
    match raw.trim() {
        "1" => Ok(true),
        "0" => Ok(false),
        other => Err(format!(
            "{file_desc}: row {row_num}: column {col:?}: expected 0 or 1, got {other:?}"
        )),
    }
}

/// Empty string -> `None`; otherwise [`parse_u64_field`].
fn parse_opt_u64_field(
    raw: &str,
    file_desc: &str,
    row_num: usize,
    col: &str,
) -> Result<Option<u64>, String> {
    if raw.trim().is_empty() {
        Ok(None)
    } else {
        parse_u64_field(raw, file_desc, row_num, col).map(Some)
    }
}

/// Empty string -> `None`; `"1"`/`"0"` -> `Some(true)`/`Some(false)` — the
/// documented `provider_match ∈ {1,0,empty}` convention exactly.
fn parse_opt_bool01_field(
    raw: &str,
    file_desc: &str,
    row_num: usize,
    col: &str,
) -> Result<Option<bool>, String> {
    match raw.trim() {
        "" => Ok(None),
        "1" => Ok(Some(true)),
        "0" => Ok(Some(false)),
        other => Err(format!(
            "{file_desc}: row {row_num}: column {col:?}: expected 0, 1, or empty, got {other:?}"
        )),
    }
}

fn parse_opt_string_field(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

// ─── (1) Trials CSV — one row per private query ────────────────────────

/// Every column `--trials` must carry, in the order the probe (`risepir-rpc
/// probe`) documents them — validated in full at header-parse time (even
/// the columns this module never reads a value from), so a header that
/// drifted from the documented contract fails loudly before any row is
/// parsed.
const TRIALS_COLUMNS: &[&str] = &[
    "batch",
    "trial",
    "started_at_unix_ms",
    "absent_probe",
    "t_total_us",
    "build_us",
    "head_wire_us",
    "sync_wire_us",
    "answer_wire_us",
    "setup_wire_us",
    "finish_us",
    "residual_us",
    "rewind_us",
    "decode_us",
    "delta_apply_us",
    "scan_us",
    "server_compute_ns",
    "server_handler_ns",
    "query_bytes",
    "response_bytes",
    "response_content_length",
    "at_block",
    "pinned_block",
    "stale_blocks",
    "delta_cells",
    "found",
    "provider_match",
    "provider_error",
    "provider_rtt_us",
    "client_rss_bytes",
    "attempts",
    "error",
];

/// One row of `--trials`: one private query. Field semantics mirror the
/// probe's own naming (`A1`..`A6` in the module that consumes these —
/// see [`render_markdown`]'s §A). Fields this module never reports a
/// statistic for (`batch`, `started_at_unix_ms`, `delta_cells`,
/// `provider_error`, `provider_rtt_us`) are still validated present in
/// the header (`TRIALS_COLUMNS`) but not extracted into this struct.
#[derive(Clone, Debug)]
pub struct TrialRow {
    /// Trial index within its batch — identifies a row in §D's
    /// data-quality notes (e.g. a `provider_match == 0` listing).
    pub trial: u64,
    /// A1: total client-observed query latency, microseconds.
    pub t_total_us: u64,
    /// A2: query-build latency, microseconds.
    pub build_us: u64,
    /// Client-measured head-fetch wire span, microseconds.
    pub head_wire_us: u64,
    /// Client-measured sync wire span, microseconds.
    pub sync_wire_us: u64,
    /// Client-measured answer wire span, microseconds.
    pub answer_wire_us: u64,
    /// The re-bootstrap `/setup` download's wire span, microseconds: `0`
    /// on a normal trial, the whole hint download on an `attempts = 2`
    /// row. A budget term of its own rather than part of the residual
    /// precisely because it is enormous when nonzero — see the probe's
    /// own module docs.
    pub setup_wire_us: u64,
    /// A5: client-side finish latency (rewind+decode+delta_apply+scan),
    /// microseconds.
    pub finish_us: u64,
    /// `t_total_us - (build+head+sync+answer+setup+finish)`, by
    /// construction — signed so a data-integrity violation is visible in
    /// either direction (see [`budget_violation_us`]).
    pub residual_us: i64,
    /// A5 sub-timer: rewind, microseconds.
    pub rewind_us: u64,
    /// A5 sub-timer: decode, microseconds.
    pub decode_us: u64,
    /// A5 sub-timer: delta apply, microseconds.
    pub delta_apply_us: u64,
    /// A5 sub-timer: scan, microseconds.
    pub scan_us: u64,
    /// A4: the server's own answer-compute time, nanoseconds. May be
    /// absent (empty column) for a campaign that did not capture server
    /// timing headers.
    pub server_compute_ns: Option<u64>,
    /// Server time from request received to response built (decode, lock
    /// wait, compute, encode), nanoseconds. May be absent, independently
    /// of `server_compute_ns`.
    pub server_handler_ns: Option<u64>,
    /// A6: client query wire size, bytes.
    pub query_bytes: u64,
    /// A6: client response wire size, bytes (as measured client-side).
    pub response_bytes: u64,
    /// A6: response `Content-Length`, bytes (as reported by the server).
    pub response_content_length: u64,
    /// Block the answer was actually served at.
    pub at_block: u64,
    /// Block the client's hint was pinned to.
    pub pinned_block: u64,
    /// `at_block - pinned_block`, as reported by the probe. Kept signed:
    /// a well-formed campaign should never see this go negative, but a
    /// raw input column is read as given, not clamped at parse time —
    /// see [`stale_bin_index`] for how a negative value bins.
    pub stale_blocks: i64,
    /// Whether the query found a nonzero/present balance.
    pub found: bool,
    /// Whether this trial deliberately probed an absent account.
    pub absent_probe: bool,
    /// Independent-provider cross-check result: `Some(true)` matched,
    /// `Some(false)` mismatched, `None` unavailable/not attempted.
    pub provider_match: Option<bool>,
    /// Sampled client RSS at the time of this trial, bytes, if sampled.
    pub client_rss_bytes: Option<u64>,
    /// Attempt count; `> 1` means an in-flight re-bootstrap happened
    /// inside `residual_us`.
    pub attempts: u32,
    /// Non-empty iff this trial failed. A failed trial's other columns
    /// (besides those describing the failure itself) do not describe a
    /// completed query and are excluded from every §A statistic — see
    /// the module docs' "What `n` counts".
    pub error: Option<String>,
}

/// Parses `--trials` content into one [`TrialRow`] per data line. Fails
/// loudly (naming the file, the line, and the column) on a missing
/// header column, a row whose field count does not match the header
/// (see `split_row`), or a value that does not parse as its documented
/// type.
pub fn parse_trials_csv(content: &str) -> Result<Vec<TrialRow>, String> {
    const FILE: &str = "--trials";
    let mut lines = csv_lines(content);
    let (_, header_line) = lines
        .next()
        .ok_or_else(|| format!("{FILE}: empty file (no header line)"))?;
    let columns = ColumnMap::from_header(header_line);
    for name in TRIALS_COLUMNS {
        columns.require(name, FILE)?;
    }
    let idx = |name: &str| columns.require(name, FILE).expect("validated above");
    let (
        i_trial,
        i_t_total,
        i_build,
        i_head,
        i_sync,
        i_answer,
        i_setup_wire,
        i_finish,
        i_residual,
        i_rewind,
        i_decode,
        i_delta_apply,
        i_scan,
        i_server_compute,
        i_server_handler,
        i_query_bytes,
        i_response_bytes,
        i_response_content_length,
        i_at_block,
        i_pinned_block,
        i_stale_blocks,
        i_found,
        i_absent_probe,
        i_provider_match,
        i_client_rss,
        i_attempts,
        i_error,
    ) = (
        idx("trial"),
        idx("t_total_us"),
        idx("build_us"),
        idx("head_wire_us"),
        idx("sync_wire_us"),
        idx("answer_wire_us"),
        idx("setup_wire_us"),
        idx("finish_us"),
        idx("residual_us"),
        idx("rewind_us"),
        idx("decode_us"),
        idx("delta_apply_us"),
        idx("scan_us"),
        idx("server_compute_ns"),
        idx("server_handler_ns"),
        idx("query_bytes"),
        idx("response_bytes"),
        idx("response_content_length"),
        idx("at_block"),
        idx("pinned_block"),
        idx("stale_blocks"),
        idx("found"),
        idx("absent_probe"),
        idx("provider_match"),
        idx("client_rss_bytes"),
        idx("attempts"),
        idx("error"),
    );

    let mut rows = Vec::new();
    for (row_num, line) in lines {
        let f = split_row(line, columns.names.len(), FILE, row_num)?;
        rows.push(TrialRow {
            trial: parse_u64_field(f[i_trial], FILE, row_num, "trial")?,
            t_total_us: parse_u64_field(f[i_t_total], FILE, row_num, "t_total_us")?,
            build_us: parse_u64_field(f[i_build], FILE, row_num, "build_us")?,
            head_wire_us: parse_u64_field(f[i_head], FILE, row_num, "head_wire_us")?,
            sync_wire_us: parse_u64_field(f[i_sync], FILE, row_num, "sync_wire_us")?,
            answer_wire_us: parse_u64_field(f[i_answer], FILE, row_num, "answer_wire_us")?,
            setup_wire_us: parse_u64_field(f[i_setup_wire], FILE, row_num, "setup_wire_us")?,
            finish_us: parse_u64_field(f[i_finish], FILE, row_num, "finish_us")?,
            residual_us: parse_i64_field(f[i_residual], FILE, row_num, "residual_us")?,
            rewind_us: parse_u64_field(f[i_rewind], FILE, row_num, "rewind_us")?,
            decode_us: parse_u64_field(f[i_decode], FILE, row_num, "decode_us")?,
            delta_apply_us: parse_u64_field(f[i_delta_apply], FILE, row_num, "delta_apply_us")?,
            scan_us: parse_u64_field(f[i_scan], FILE, row_num, "scan_us")?,
            server_compute_ns: parse_opt_u64_field(
                f[i_server_compute],
                FILE,
                row_num,
                "server_compute_ns",
            )?,
            server_handler_ns: parse_opt_u64_field(
                f[i_server_handler],
                FILE,
                row_num,
                "server_handler_ns",
            )?,
            query_bytes: parse_u64_field(f[i_query_bytes], FILE, row_num, "query_bytes")?,
            response_bytes: parse_u64_field(f[i_response_bytes], FILE, row_num, "response_bytes")?,
            response_content_length: parse_u64_field(
                f[i_response_content_length],
                FILE,
                row_num,
                "response_content_length",
            )?,
            at_block: parse_u64_field(f[i_at_block], FILE, row_num, "at_block")?,
            pinned_block: parse_u64_field(f[i_pinned_block], FILE, row_num, "pinned_block")?,
            stale_blocks: parse_i64_field(f[i_stale_blocks], FILE, row_num, "stale_blocks")?,
            found: parse_bool01_field(f[i_found], FILE, row_num, "found")?,
            absent_probe: parse_bool01_field(f[i_absent_probe], FILE, row_num, "absent_probe")?,
            provider_match: parse_opt_bool01_field(
                f[i_provider_match],
                FILE,
                row_num,
                "provider_match",
            )?,
            client_rss_bytes: parse_opt_u64_field(
                f[i_client_rss],
                FILE,
                row_num,
                "client_rss_bytes",
            )?,
            attempts: parse_u32_field(f[i_attempts], FILE, row_num, "attempts")?,
            error: parse_opt_string_field(f[i_error]),
        });
    }
    Ok(rows)
}

// ─── (2) Client-blocks CSV — one row per client-observed delta fetch ───

/// Every column `--client-blocks` must carry.
const CLIENT_BLOCKS_COLUMNS: &[&str] = &[
    "block",
    "received_at_unix_ms",
    "wire_bytes",
    "decode_us",
    "ingest_us",
    "cells_in_block",
    "delta_cells_total",
    "fetch_wire_us",
    "blocks_in_fetch",
];

/// One row of `--client-blocks`. `wire_bytes`/`ingest_us` cover the whole
/// coalesced fetch when `blocks_in_fetch > 1`; §B9/§B10's per-block
/// statistics use only rows with `blocks_in_fetch == 1` — see
/// [`single_block_client_rows`].
#[derive(Clone, Debug)]
pub struct ClientBlockRow {
    /// Block number this fetch reached (or the sole block, if
    /// `blocks_in_fetch == 1`).
    pub block: u64,
    /// Wire bytes for this fetch (covers every coalesced block if
    /// `blocks_in_fetch > 1`).
    pub wire_bytes: u64,
    /// Client-side delta decode time, microseconds.
    pub decode_us: u64,
    /// Client-side delta ingest time, microseconds.
    pub ingest_us: u64,
    /// Number of blocks this fetch's `wire_bytes`/`decode_us`/`ingest_us`
    /// actually cover; `1` for an uncoalesced single-block fetch.
    pub blocks_in_fetch: u32,
}

/// Parses `--client-blocks` content into one [`ClientBlockRow`] per data
/// line, with the same by-name, fail-loud parsing as [`parse_trials_csv`].
pub fn parse_client_blocks_csv(content: &str) -> Result<Vec<ClientBlockRow>, String> {
    const FILE: &str = "--client-blocks";
    let mut lines = csv_lines(content);
    let (_, header_line) = lines
        .next()
        .ok_or_else(|| format!("{FILE}: empty file (no header line)"))?;
    let columns = ColumnMap::from_header(header_line);
    for name in CLIENT_BLOCKS_COLUMNS {
        columns.require(name, FILE)?;
    }
    let idx = |name: &str| columns.require(name, FILE).expect("validated above");
    let (i_block, i_wire_bytes, i_decode, i_ingest, i_blocks_in_fetch) = (
        idx("block"),
        idx("wire_bytes"),
        idx("decode_us"),
        idx("ingest_us"),
        idx("blocks_in_fetch"),
    );

    let mut rows = Vec::new();
    for (row_num, line) in lines {
        let f = split_row(line, columns.names.len(), FILE, row_num)?;
        rows.push(ClientBlockRow {
            block: parse_u64_field(f[i_block], FILE, row_num, "block")?,
            wire_bytes: parse_u64_field(f[i_wire_bytes], FILE, row_num, "wire_bytes")?,
            decode_us: parse_u64_field(f[i_decode], FILE, row_num, "decode_us")?,
            ingest_us: parse_u64_field(f[i_ingest], FILE, row_num, "ingest_us")?,
            blocks_in_fetch: parse_u32_field(
                f[i_blocks_in_fetch],
                FILE,
                row_num,
                "blocks_in_fetch",
            )?,
        });
    }
    Ok(rows)
}

/// Rows with `blocks_in_fetch == 1` — the population §B9/§B10's
/// per-block client statistics use, per the module docs.
pub fn single_block_client_rows(rows: &[ClientBlockRow]) -> Vec<&ClientBlockRow> {
    rows.iter().filter(|r| r.blocks_in_fetch == 1).collect()
}

/// Count of rows excluded from the single-block statistics because they
/// coalesced more than one block (`blocks_in_fetch != 1`).
pub fn coalesced_client_row_count(rows: &[ClientBlockRow]) -> usize {
    rows.iter().filter(|r| r.blocks_in_fetch != 1).count()
}

// ─── (3) Server-blocks CSV — one row per block applied by the server ───

/// Every column `--server-blocks` must carry. Kept as a `const` list
/// specifically because this header is the one the module docs (and the
/// task brief) flag as likely to be renamed once the server branch that
/// produces it lands — updating this list is then the one line that
/// needs to change; every lookup below already goes through
/// [`ColumnMap::require`] by name, never by position.
const SERVER_BLOCKS_COLUMNS: &[&str] = &[
    "block",
    "applied_at_unix_ms",
    "changes",
    "credits",
    "inserts",
    "updates",
    "deletes",
    "noop_deletes",
    "touched_cells",
    "store_ms",
    "fold_ms",
    "patch_ms",
    "apply_ms",
    "lock_wait_ms",
    "delta_bytes",
    "answers_since_prev_block",
    "answer_compute_ms_since_prev_block",
    "feed_fetch_ms",
    "finalized_block",
];

/// One row of `--server-blocks`: one block applied by the server.
#[derive(Clone, Debug)]
pub struct ServerBlockRow {
    /// Block number.
    pub block: u64,
    /// Source change count for this block (credits + the B7 mutations).
    pub changes: u64,
    /// Credit count within `changes`.
    pub credits: u64,
    /// B7: insert count.
    pub inserts: u64,
    /// B7: update count.
    pub updates: u64,
    /// B7: delete count.
    pub deletes: u64,
    /// Deletes of keys not present (no-op), reported separately from B7.
    pub noop_deletes: u64,
    /// Store cells touched applying this block.
    pub touched_cells: u64,
    /// B8 stage: store time, milliseconds.
    pub store_ms: f64,
    /// B8 stage: fold time, milliseconds.
    pub fold_ms: f64,
    /// B8 stage: patch time, milliseconds.
    pub patch_ms: f64,
    /// B8: total apply time, milliseconds (store+fold+patch by
    /// construction; the gap from that sum is reported as a derived
    /// residual — see [`render_markdown`]'s §B8).
    pub apply_ms: f64,
    /// Write-lock wait time applying this block, milliseconds.
    pub lock_wait_ms: f64,
    /// B9: server-side delta wire size, bytes.
    pub delta_bytes: u64,
    /// Answers served since the previous block — `Some(n > 0)` marks this
    /// block as applied while probe traffic was active (the interference
    /// check's "probe-adjacent" subset).
    ///
    /// `None` (an empty CSV field) on the **first block a follow-loop run
    /// applies**: that row's "since" window would otherwise start at loop
    /// entry rather than at a genuine previous applied block, which the
    /// producer deliberately refuses to report as the same quantity. Such
    /// a row is neither quiet nor probe-adjacent — it is unknown — so it
    /// is excluded from both subsets below rather than silently counted
    /// as quiet.
    pub answers_since_prev_block: Option<u64>,
}

/// Parses `--server-blocks` content into one [`ServerBlockRow`] per data
/// line, with the same by-name, fail-loud parsing as [`parse_trials_csv`].
pub fn parse_server_blocks_csv(content: &str) -> Result<Vec<ServerBlockRow>, String> {
    const FILE: &str = "--server-blocks";
    let mut lines = csv_lines(content);
    let (_, header_line) = lines
        .next()
        .ok_or_else(|| format!("{FILE}: empty file (no header line)"))?;
    let columns = ColumnMap::from_header(header_line);
    for name in SERVER_BLOCKS_COLUMNS {
        columns.require(name, FILE)?;
    }
    let idx = |name: &str| columns.require(name, FILE).expect("validated above");
    let (
        i_block,
        i_changes,
        i_credits,
        i_inserts,
        i_updates,
        i_deletes,
        i_noop_deletes,
        i_touched_cells,
        i_store_ms,
        i_fold_ms,
        i_patch_ms,
        i_apply_ms,
        i_lock_wait_ms,
        i_delta_bytes,
        i_answers_since_prev_block,
    ) = (
        idx("block"),
        idx("changes"),
        idx("credits"),
        idx("inserts"),
        idx("updates"),
        idx("deletes"),
        idx("noop_deletes"),
        idx("touched_cells"),
        idx("store_ms"),
        idx("fold_ms"),
        idx("patch_ms"),
        idx("apply_ms"),
        idx("lock_wait_ms"),
        idx("delta_bytes"),
        idx("answers_since_prev_block"),
    );

    let mut rows = Vec::new();
    for (row_num, line) in lines {
        let f = split_row(line, columns.names.len(), FILE, row_num)?;
        rows.push(ServerBlockRow {
            block: parse_u64_field(f[i_block], FILE, row_num, "block")?,
            changes: parse_u64_field(f[i_changes], FILE, row_num, "changes")?,
            credits: parse_u64_field(f[i_credits], FILE, row_num, "credits")?,
            inserts: parse_u64_field(f[i_inserts], FILE, row_num, "inserts")?,
            updates: parse_u64_field(f[i_updates], FILE, row_num, "updates")?,
            deletes: parse_u64_field(f[i_deletes], FILE, row_num, "deletes")?,
            noop_deletes: parse_u64_field(f[i_noop_deletes], FILE, row_num, "noop_deletes")?,
            touched_cells: parse_u64_field(f[i_touched_cells], FILE, row_num, "touched_cells")?,
            store_ms: parse_f64_field(f[i_store_ms], FILE, row_num, "store_ms")?,
            fold_ms: parse_f64_field(f[i_fold_ms], FILE, row_num, "fold_ms")?,
            patch_ms: parse_f64_field(f[i_patch_ms], FILE, row_num, "patch_ms")?,
            apply_ms: parse_f64_field(f[i_apply_ms], FILE, row_num, "apply_ms")?,
            lock_wait_ms: parse_f64_field(f[i_lock_wait_ms], FILE, row_num, "lock_wait_ms")?,
            delta_bytes: parse_u64_field(f[i_delta_bytes], FILE, row_num, "delta_bytes")?,
            answers_since_prev_block: parse_opt_u64_field(
                f[i_answers_since_prev_block],
                FILE,
                row_num,
                "answers_since_prev_block",
            )?,
        });
    }
    Ok(rows)
}

// ─── (4) Setup JSON ─────────────────────────────────────────────────────

fn json_object(
    content: &str,
    file_desc: &str,
) -> Result<serde_json::Map<String, serde_json::Value>, String> {
    let value: serde_json::Value =
        serde_json::from_str(content).map_err(|e| format!("{file_desc}: invalid JSON: {e}"))?;
    match value {
        serde_json::Value::Object(map) => Ok(map),
        _ => Err(format!("{file_desc}: JSON top level must be an object")),
    }
}

fn json_field<'a>(
    obj: &'a serde_json::Map<String, serde_json::Value>,
    key: &str,
    file_desc: &str,
) -> Result<&'a serde_json::Value, String> {
    obj.get(key)
        .ok_or_else(|| format!("{file_desc}: missing required field {key:?}"))
}

fn json_u64(
    obj: &serde_json::Map<String, serde_json::Value>,
    key: &str,
    file_desc: &str,
) -> Result<u64, String> {
    json_field(obj, key, file_desc)?
        .as_u64()
        .ok_or_else(|| format!("{file_desc}: field {key:?} is not a non-negative integer"))
}

fn json_u32(
    obj: &serde_json::Map<String, serde_json::Value>,
    key: &str,
    file_desc: &str,
) -> Result<u32, String> {
    let v = json_u64(obj, key, file_desc)?;
    u32::try_from(v).map_err(|_| format!("{file_desc}: field {key:?} ({v}) does not fit in u32"))
}

fn json_f64(
    obj: &serde_json::Map<String, serde_json::Value>,
    key: &str,
    file_desc: &str,
) -> Result<f64, String> {
    json_field(obj, key, file_desc)?
        .as_f64()
        .ok_or_else(|| format!("{file_desc}: field {key:?} is not a number"))
}

fn json_bool(
    obj: &serde_json::Map<String, serde_json::Value>,
    key: &str,
    file_desc: &str,
) -> Result<bool, String> {
    json_field(obj, key, file_desc)?
        .as_bool()
        .ok_or_else(|| format!("{file_desc}: field {key:?} is not a boolean"))
}

/// One `--setup` measurement (from `risepir-rpc time-setup`): C11's
/// served scale and DB/hint size, plus C13's one-time setup measurement
/// and invariant check.
#[derive(Clone, Debug)]
pub struct SetupInfo {
    /// C11: account count this setup served.
    pub accounts: u64,
    /// `Geometry::num_buckets` for this setup.
    pub buckets: u32,
    /// C11: measured whole-store DB size, bytes.
    pub cells_bytes: u64,
    /// C11: measured whole-deployment hint size, bytes.
    pub hint_bytes: u64,
    /// `Geometry::arity`.
    pub arity: u32,
    /// `Geometry::bucket_size`.
    pub bucket_size: u32,
    /// LWE dimension used for this setup.
    pub lwe_dim: u32,
    /// `Geometry::plaintext_bits`, as measured (not re-derived).
    pub plaintext_bits: u32,
    /// C13: one-time PIR setup wall time, seconds.
    pub setup_seconds: f64,
    /// C13: whether the persisted hints reproduced **byte for byte** from
    /// the persisted seed and the store's current cells — the invariant
    /// check, and the only field of `risepir-rpc time-setup`'s JSON that
    /// gates its exit code. Named exactly as the producer writes it
    /// (`persisted_hints_exact_match`); it was `hints_match_persisted`
    /// before that check was made exact.
    pub persisted_hints_exact_match: bool,
    /// Block this setup measurement was taken at/after.
    pub state_block: u64,
    /// C13: rayon thread count used for this setup.
    pub rayon_threads: u32,
}

/// Parses `--setup` JSON content into a [`SetupInfo`]. Fails loudly on a
/// missing or wrongly-typed field.
pub fn parse_setup_json(content: &str) -> Result<SetupInfo, String> {
    const FILE: &str = "--setup";
    let obj = json_object(content, FILE)?;
    Ok(SetupInfo {
        accounts: json_u64(&obj, "accounts", FILE)?,
        buckets: json_u32(&obj, "buckets", FILE)?,
        cells_bytes: json_u64(&obj, "cells_bytes", FILE)?,
        hint_bytes: json_u64(&obj, "hint_bytes", FILE)?,
        arity: json_u32(&obj, "arity", FILE)?,
        bucket_size: json_u32(&obj, "bucket_size", FILE)?,
        lwe_dim: json_u32(&obj, "lwe_dim", FILE)?,
        plaintext_bits: json_u32(&obj, "plaintext_bits", FILE)?,
        setup_seconds: json_f64(&obj, "setup_seconds", FILE)?,
        persisted_hints_exact_match: json_bool(&obj, "persisted_hints_exact_match", FILE)?,
        state_block: json_u64(&obj, "state_block", FILE)?,
        rayon_threads: json_u32(&obj, "rayon_threads", FILE)?,
    })
}

/// Builds the [`Geometry`] a [`SetupInfo`] measured, for [`Geometry::sizes`]
/// comparisons (§A6, §C11). `arity`/`num_buckets`/`bucket_size`/
/// `plaintext_bits` all come from the campaign's own measurement, never
/// hardcoded; `fingerprint_bits`/`value_bits` are this deployment's fixed
/// value-codec layout (ADR-0009), the same constants `xtask::bench` fixes
/// for the identical reason (they are not part of the geometry a campaign
/// varies — the SCF's key/value encoding is fixed workspace-wide).
pub fn setup_geometry(setup: &SetupInfo) -> Geometry {
    Geometry {
        arity: setup.arity,
        num_buckets: setup.buckets,
        bucket_size: setup.bucket_size,
        fingerprint_bits: FINGERPRINT_BITS,
        value_bits: value_codec().value_bits(),
        plaintext_bits: setup.plaintext_bits,
    }
}

/// One `--setup-download` measurement (C12): the `/setup` download the
/// probe records at startup, which the `--setup`/`time-setup` JSON does
/// not carry.
#[derive(Clone, Debug)]
pub struct SetupDownloadInfo {
    /// Measured `/setup` response size, bytes.
    pub setup_bytes: u64,
    /// Measured `Content-Length` header, bytes.
    pub content_length: u64,
    /// Measured wall-clock download time, seconds.
    pub wall_seconds: f64,
    /// Block the downloaded setup was pinned to.
    pub pinned_block: u64,
}

/// Parses `--setup-download` JSON content into a [`SetupDownloadInfo`].
pub fn parse_setup_download_json(content: &str) -> Result<SetupDownloadInfo, String> {
    const FILE: &str = "--setup-download";
    let obj = json_object(content, FILE)?;
    Ok(SetupDownloadInfo {
        setup_bytes: json_u64(&obj, "setup_bytes", FILE)?,
        content_length: json_u64(&obj, "content_length", FILE)?,
        wall_seconds: json_f64(&obj, "wall_seconds", FILE)?,
        pinned_block: json_u64(&obj, "pinned_block", FILE)?,
    })
}

// ─── (5) Provenance — free-form key/values, printed verbatim ───────────

/// Parses `--provenance` content into an ordered list of `(key, value)`
/// pairs to print verbatim (§0) — this module never interprets or
/// invents any of them. `is_json` selects the format (the caller sniffs
/// this from the path's extension); JSON is parsed with `serde_json`
/// (object key order, which — without the `preserve_order` feature — is
/// sorted by key), otherwise a minimal flat `key = value` reader handles
/// the common TOML shape this tool actually receives.
pub fn parse_provenance(content: &str, is_json: bool) -> Result<Vec<(String, String)>, String> {
    if is_json {
        parse_provenance_json(content)
    } else {
        parse_provenance_toml_like(content)
    }
}

fn parse_provenance_json(content: &str) -> Result<Vec<(String, String)>, String> {
    const FILE: &str = "--provenance (json)";
    let obj = json_object(content, FILE)?;
    Ok(obj
        .iter()
        .map(|(k, v)| (k.clone(), json_value_to_display(v)))
        .collect())
}

fn json_value_to_display(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// Minimal flat `key = value` reader — not a general TOML parser. One
/// assignment per non-blank, non-comment (`#`) line; a `[section]`
/// heading line is skipped rather than rejected; one layer of surrounding
/// `"`/`'` quotes is stripped from the value. Sufficient for the flat
/// provenance files this tool actually receives (commit, geometry, host,
/// client vantage, link, block range, UTC window) — a nested table is out
/// of scope, since the provenance data itself is documented as flat
/// key/values; use a `.json` provenance file instead if that is ever
/// needed.
fn parse_provenance_toml_like(content: &str) -> Result<Vec<(String, String)>, String> {
    let mut out = Vec::new();
    for (line_num, raw_line) in content.lines().enumerate() {
        let line = raw_line.trim();
        if line.is_empty()
            || line.starts_with('#')
            || (line.starts_with('[') && line.ends_with(']'))
        {
            continue;
        }
        let (key, value) = line.split_once('=').ok_or_else(|| {
            format!(
                "--provenance (toml): line {}: expected `key = value`, got {raw_line:?}",
                line_num + 1
            )
        })?;
        let key = key.trim().to_string();
        let mut value = value.trim();
        let bytes = value.as_bytes();
        if bytes.len() >= 2
            && ((bytes[0] == b'"' && bytes[bytes.len() - 1] == b'"')
                || (bytes[0] == b'\'' && bytes[bytes.len() - 1] == b'\''))
        {
            value = &value[1..value.len() - 1];
        }
        out.push((key, value.to_string()));
    }
    Ok(out)
}

// ─── Report data bundle ─────────────────────────────────────────────────

/// Every parsed input `render_markdown` needs. Construct with
/// [`ReportData::parse`].
#[derive(Clone, Debug)]
pub struct ReportData {
    /// Parsed `--trials`.
    pub trials: Vec<TrialRow>,
    /// Parsed `--client-blocks`.
    pub client_blocks: Vec<ClientBlockRow>,
    /// Parsed `--server-blocks`.
    pub server_blocks: Vec<ServerBlockRow>,
    /// Parsed `--setup`.
    pub setup: SetupInfo,
    /// Parsed `--setup-download`, if given.
    pub setup_download: Option<SetupDownloadInfo>,
    /// Parsed `--provenance`, in file order.
    pub provenance: Vec<(String, String)>,
}

impl ReportData {
    /// Parses every input. `provenance_is_json` selects
    /// [`parse_provenance`]'s format.
    #[allow(clippy::too_many_arguments)]
    pub fn parse(
        trials_csv: &str,
        client_blocks_csv: &str,
        server_blocks_csv: &str,
        setup_json: &str,
        setup_download_json: Option<&str>,
        provenance_raw: &str,
        provenance_is_json: bool,
    ) -> Result<Self, String> {
        Ok(Self {
            trials: parse_trials_csv(trials_csv)?,
            client_blocks: parse_client_blocks_csv(client_blocks_csv)?,
            server_blocks: parse_server_blocks_csv(server_blocks_csv)?,
            setup: parse_setup_json(setup_json)?,
            setup_download: setup_download_json
                .map(parse_setup_download_json)
                .transpose()?,
            provenance: parse_provenance(provenance_raw, provenance_is_json)?,
        })
    }
}

// ─── Derived helpers shared across sections ─────────────────────────────

/// Trial rows without an `error` — the population every §A statistic is
/// computed over (see the module docs' "What `n` counts").
pub fn successful_trials(trials: &[TrialRow]) -> Vec<&TrialRow> {
    trials.iter().filter(|t| t.error.is_none()).collect()
}

/// The staleness bin edges §A bins `stale_blocks` into, `(label, lo, hi)`
/// inclusive on both ends.
pub const STALE_BINS: [(&str, i64, i64); 5] = [
    ("0-99", 0, 99),
    ("100-299", 100, 299),
    ("300-599", 300, 599),
    ("600-899", 600, 899),
    ("900+", 900, i64::MAX),
];

/// Index into [`STALE_BINS`] for one `stale_blocks` value. A negative
/// value (which a well-formed campaign should never produce, since block
/// numbers only advance — but this module reads `stale_blocks` as given,
/// not clamped at parse time) is treated as "not stale" and falls into
/// the first bin, rather than panicking or being silently dropped.
pub fn stale_bin_index(stale_blocks: i64) -> usize {
    let v = stale_blocks.max(0);
    STALE_BINS
        .iter()
        .position(|&(_, lo, hi)| v >= lo && v <= hi)
        .unwrap_or(STALE_BINS.len() - 1)
}

/// `values` grouped into [`STALE_BINS`] by `extract(row)`'s
/// `stale_blocks`, each bin's [`Stats`] computed over `metric(row)`.
fn binned_stats<'a>(
    rows: &[&'a TrialRow],
    metric: impl Fn(&'a TrialRow) -> f64,
) -> [Option<Stats>; STALE_BINS.len()] {
    let mut buckets: [Vec<f64>; STALE_BINS.len()] = std::array::from_fn(|_| Vec::new());
    for &row in rows {
        buckets[stale_bin_index(row.stale_blocks)].push(metric(row));
    }
    buckets.map(|b| compute_stats(&b))
}

/// `A1 − (A2 + head + sync + answer + setup + A5 + residual)`, in
/// microseconds, for one trial row. Must be exactly `0` for a well-formed
/// campaign — `residual_us` is defined as exactly that difference by the
/// producer (`risepir-rpc probe`), so a nonzero value reports a
/// data-integrity problem in the raw CSV, not a bug in this computation.
///
/// `setup_wire_us` is a term here because the producer makes it one: it
/// is `0` on every normal trial but carries the whole re-bootstrap
/// `/setup` download on an `attempts = 2` row, and omitting it would
/// report exactly those rows — the rare, interesting ones — as spurious
/// budget violations.
pub fn budget_violation_us(row: &TrialRow) -> i64 {
    let lhs = row.t_total_us as i64;
    let rhs = row.build_us as i64
        + row.head_wire_us as i64
        + row.sync_wire_us as i64
        + row.answer_wire_us as i64
        + row.setup_wire_us as i64
        + row.finish_us as i64
        + row.residual_us;
    lhs - rhs
}

/// Max `|budget_violation_us|` across `rows`, and the `trial` of the
/// worst offender (`None` iff `rows` is empty or every row balances
/// exactly). Never aborts the report on a nonzero result — see
/// [`render_markdown`]'s §A budget table and §D, which both print this
/// value; a violation is a finding about the campaign's data, exactly
/// what §D exists to surface.
pub fn max_budget_violation(rows: &[&TrialRow]) -> (i64, Option<u64>) {
    let worst = rows
        .iter()
        .map(|r| (budget_violation_us(r), r.trial))
        .max_by_key(|&(v, _)| v.unsigned_abs());
    match worst {
        // A `Some` violation of exactly `0` still means every row balanced
        // exactly — there is no "offending" trial to name, so this must
        // report `None` here too, not the last-seen trial id.
        Some((v, trial)) if v != 0 => (v, Some(trial)),
        _ => (0, None),
    }
}

/// Rows where both `server_compute_ns` and `server_handler_ns` are
/// present — the only population A3 / "server handler overhead" can be
/// computed from, since either header may be empty for a whole campaign
/// or, in principle, for one row.
fn rows_with_server_headers<'a>(rows: &[&'a TrialRow]) -> Vec<&'a TrialRow> {
    rows.iter()
        .copied()
        .filter(|r| r.server_compute_ns.is_some() && r.server_handler_ns.is_some())
        .collect()
}

// ─── Formatting helpers ─────────────────────────────────────────────────

/// `v` formatted with exactly `decimals` fractional digits.
fn fmt_dec(v: f64, decimals: usize) -> String {
    format!("{v:.decimals$}")
}

fn write_stats_row(out: &mut String, label: &str, stats: Option<Stats>, decimals: usize) {
    match stats {
        Some(s) => writeln!(
            out,
            "| {label} | {} | {} | {} | {} | {} | {} |",
            s.n,
            fmt_dec(s.mean, decimals),
            fmt_dec(s.p50, decimals),
            fmt_dec(s.p95, decimals),
            fmt_dec(s.min, decimals),
            fmt_dec(s.max, decimals),
        )
        .unwrap(),
        None => writeln!(out, "| {label} | 0 | — | — | — | — | — |").unwrap(),
    }
}

fn write_bytes_stats_row(out: &mut String, label: &str, stats: Option<Stats>) {
    match stats {
        Some(s) => writeln!(
            out,
            "| {label} | {} | {} | {} | {} |",
            s.n,
            fmt_bytes(s.mean.round() as u64),
            fmt_bytes(s.min.round() as u64),
            fmt_bytes(s.max.round() as u64),
        )
        .unwrap(),
        None => writeln!(out, "| {label} | 0 | — | — | — |").unwrap(),
    }
}

fn write_count_stats_row_with_total(out: &mut String, label: &str, values_u64: &[u64]) {
    let values: Vec<f64> = values_u64.iter().map(|&v| v as f64).collect();
    let total: u64 = values_u64.iter().sum();
    match compute_stats(&values) {
        Some(s) => writeln!(
            out,
            "| {label} | {} | {} | {} | {} | {} | {} | {} |",
            s.n,
            fmt_dec(s.mean, 2),
            fmt_dec(s.p50, 0),
            fmt_dec(s.p95, 0),
            fmt_dec(s.min, 0),
            fmt_dec(s.max, 0),
            fmt_num(total),
        )
        .unwrap(),
        None => writeln!(
            out,
            "| {label} | 0 | — | — | — | — | — | {} |",
            fmt_num(total)
        )
        .unwrap(),
    }
}

// ─── Rendering ────────────────────────────────────────────────────────────

/// Renders every parsed input into the full markdown report: §0
/// provenance/method, §A one private query, §B one block, §C one time,
/// §D interference and data-quality notes. See the module docs for the
/// `(measured)`/`(computed)`/`(derived)` labeling convention and the
/// percentile method.
pub fn render_markdown(data: &ReportData) -> String {
    let mut out = String::new();
    let successful = successful_trials(&data.trials);
    let errored_count = data.trials.len() - successful.len();

    render_section_0(&mut out, data, &successful);
    render_section_a(&mut out, data, &successful);
    render_section_b(&mut out, data);
    render_section_c(&mut out, data);
    render_section_d(&mut out, data, &successful, errored_count);

    out
}

fn render_section_0(out: &mut String, data: &ReportData, successful: &[&TrialRow]) {
    writeln!(out, "# RisePIR measurement campaign report").unwrap();
    writeln!(out).unwrap();
    writeln!(out, "## 0. Provenance and method").unwrap();
    writeln!(out).unwrap();
    writeln!(
        out,
        "**Provenance**, as given — this tool prints these fields verbatim; it never invents one."
    )
    .unwrap();
    writeln!(out).unwrap();
    if data.provenance.is_empty() {
        writeln!(out, "(no provenance fields given)").unwrap();
    } else {
        writeln!(out, "| field | value |").unwrap();
        writeln!(out, "|---|---|").unwrap();
        for (k, v) in &data.provenance {
            writeln!(out, "| {k} | {v} |").unwrap();
        }
    }
    writeln!(out).unwrap();
    writeln!(
        out,
        "**Method.** Percentiles use the nearest-rank method on the sorted successful samples: \
         for quantile `q` and `n` samples, the reported value is the sample at 1-based rank \
         `ceil(q*n)` (0-based index `ceil(q*n) - 1`) — an actual sample, never interpolated. \
         `n` counts only `--trials` rows whose `error` column is empty; a nonempty `error` \
         excludes that row from every §A statistic (§D reports errored rows separately). The \
         `--client-blocks`/`--server-blocks` CSVs carry no `error` column, so their `n` is every \
         parsed row (further filtered to single-block fetches for §B9/§B10)."
    )
    .unwrap();
    writeln!(out).unwrap();

    let pinned_stats = stats_from(successful.iter().copied(), |r| r.pinned_block as f64);
    let stale_stats = stats_from(successful.iter().copied(), |r| r.stale_blocks as f64);
    match (pinned_stats, stale_stats) {
        (Some(p), Some(s)) => writeln!(
            out,
            "**Staleness operating point** (measured, over {} successful trials): `pinned_block` \
             ranges {}–{}; `stale_blocks` ranges {}–{}.",
            p.n,
            fmt_num(p.min as u64),
            fmt_num(p.max as u64),
            s.min as i64,
            s.max as i64,
        )
        .unwrap(),
        _ => writeln!(out, "**Staleness operating point:** no successful trials.").unwrap(),
    }
    writeln!(out).unwrap();
}

fn render_section_a(out: &mut String, data: &ReportData, successful: &[&TrialRow]) {
    writeln!(out, "## A. One private query").unwrap();
    writeln!(out).unwrap();

    writeln!(out, "### A1\u{2013}A6: per-query timing ({MEASURED})").unwrap();
    writeln!(out).unwrap();
    writeln!(
        out,
        "| component | n | mean (us) | p50 (us) | p95 (us) | min (us) | max (us) |"
    )
    .unwrap();
    writeln!(out, "|---|---:|---:|---:|---:|---:|---:|").unwrap();
    write_stats_row(
        out,
        "A1 (t_total_us)",
        stats_from(successful.iter().copied(), |r| r.t_total_us as f64),
        2,
    );
    write_stats_row(
        out,
        "A2 (build_us)",
        stats_from(successful.iter().copied(), |r| r.build_us as f64),
        2,
    );
    write_stats_row(
        out,
        "head_wire_us",
        stats_from(successful.iter().copied(), |r| r.head_wire_us as f64),
        2,
    );
    write_stats_row(
        out,
        "sync_wire_us",
        stats_from(successful.iter().copied(), |r| r.sync_wire_us as f64),
        2,
    );
    write_stats_row(
        out,
        "answer_wire_us",
        stats_from(successful.iter().copied(), |r| r.answer_wire_us as f64),
        2,
    );
    write_stats_row(
        out,
        "A5 (finish_us)",
        stats_from(successful.iter().copied(), |r| r.finish_us as f64),
        2,
    );
    writeln!(out).unwrap();

    // ── Budget identity ──
    writeln!(out, "### Budget identity ({DERIVED})").unwrap();
    writeln!(out).unwrap();
    writeln!(
        out,
        "`A1 = A2 + head_wire_us + sync_wire_us + answer_wire_us + setup_wire_us + A5 + residual_us`"
    )
    .unwrap();
    writeln!(out).unwrap();
    writeln!(out, "| component | mean (us) |").unwrap();
    writeln!(out, "|---|---:|").unwrap();
    let mean_of =
        |f: fn(&TrialRow) -> f64| stats_from(successful.iter().copied(), f).map_or(0.0, |s| s.mean);
    let a2_mean = mean_of(|r| r.build_us as f64);
    let head_mean = mean_of(|r| r.head_wire_us as f64);
    let sync_mean = mean_of(|r| r.sync_wire_us as f64);
    let answer_mean = mean_of(|r| r.answer_wire_us as f64);
    let setup_wire_mean = mean_of(|r| r.setup_wire_us as f64);
    let a5_mean = mean_of(|r| r.finish_us as f64);
    let residual_mean = mean_of(|r| r.residual_us as f64);
    let a1_mean = mean_of(|r| r.t_total_us as f64);
    writeln!(out, "| A2 (build_us) | {} |", fmt_dec(a2_mean, 2)).unwrap();
    writeln!(out, "| head_wire_us | {} |", fmt_dec(head_mean, 2)).unwrap();
    writeln!(out, "| sync_wire_us | {} |", fmt_dec(sync_mean, 2)).unwrap();
    writeln!(out, "| answer_wire_us | {} |", fmt_dec(answer_mean, 2)).unwrap();
    writeln!(out, "| setup_wire_us | {} |", fmt_dec(setup_wire_mean, 2)).unwrap();
    writeln!(out, "| A5 (finish_us) | {} |", fmt_dec(a5_mean, 2)).unwrap();
    writeln!(out, "| residual_us | {} |", fmt_dec(residual_mean, 2)).unwrap();
    let sum_of_means =
        a2_mean + head_mean + sync_mean + answer_mean + setup_wire_mean + a5_mean + residual_mean;
    writeln!(
        out,
        "| **sum of components** ({DERIVED}) | {} |",
        fmt_dec(sum_of_means, 2)
    )
    .unwrap();
    writeln!(
        out,
        "| A1 (t_total_us) mean, for comparison | {} |",
        fmt_dec(a1_mean, 2)
    )
    .unwrap();
    writeln!(out).unwrap();
    let (max_violation, worst_trial) = max_budget_violation(successful);
    let violation_note = if max_violation == 0 {
        "identity holds exactly".to_string()
    } else {
        format!(
            "VIOLATION — see \u{a7}D (worst at trial {})",
            worst_trial.map_or("?".to_string(), |t| t.to_string())
        )
    };
    writeln!(
        out,
        "Per-row check ({DERIVED}): max |A1 \u{2212} (A2+head+sync+answer+setup+A5+residual)| \
         across {} \
         successful rows = {} us ({violation_note}).",
        successful.len(),
        max_violation.unsigned_abs(),
    )
    .unwrap();
    writeln!(out).unwrap();

    // ── A4 / A3 / server handler overhead ──
    writeln!(
        out,
        "### A3, A4, and server handler overhead ({MEASURED}, {DERIVED})"
    )
    .unwrap();
    writeln!(out).unwrap();
    let with_headers = rows_with_server_headers(successful);
    if with_headers.is_empty() {
        writeln!(
            out,
            "No row in this dataset carries both `server_compute_ns` and `server_handler_ns`; \
             A4, A3, and server handler overhead are not computed."
        )
        .unwrap();
    } else {
        writeln!(
            out,
            "Server timing headers present for {}/{} successful rows.",
            with_headers.len(),
            successful.len()
        )
        .unwrap();
        writeln!(out).unwrap();

        // A4 is the only *measured* distribution in this section — the
        // server's own `x-risepir-answer-compute-ns` header, rendered in
        // milliseconds (not microseconds, unlike the wire components
        // above) to match B8's stage-timer convention for compute costs.
        writeln!(out, "**A4 ({MEASURED}):**").unwrap();
        writeln!(out).unwrap();
        writeln!(
            out,
            "| quantity | n | mean (ms) | p50 (ms) | p95 (ms) | min (ms) | max (ms) |"
        )
        .unwrap();
        writeln!(out, "|---|---:|---:|---:|---:|---:|---:|").unwrap();
        let a4 = stats_from(with_headers.iter().copied(), |r| {
            r.server_compute_ns.expect("filtered above") as f64 / 1_000_000.0
        });
        write_stats_row(out, "A4 (server_compute_ns)", a4, 4);
        writeln!(out).unwrap();

        writeln!(out, "**A3 and server handler overhead ({DERIVED}):**").unwrap();
        writeln!(out).unwrap();
        writeln!(
            out,
            "| quantity | n | mean (us) | p50 (us) | p95 (us) | min (us) | max (us) |"
        )
        .unwrap();
        writeln!(out, "|---|---:|---:|---:|---:|---:|---:|").unwrap();
        let a3 = stats_from(with_headers.iter().copied(), |r| {
            let handler_us = r.server_handler_ns.expect("filtered above") as f64 / 1000.0;
            (r.head_wire_us + r.sync_wire_us + r.answer_wire_us) as f64 - handler_us
        });
        write_stats_row(
            out,
            "A3 = (head+sync+answer wire) \u{2212} server_handler",
            a3,
            2,
        );
        let overhead = stats_from(with_headers.iter().copied(), |r| {
            let handler_us = r.server_handler_ns.expect("filtered above") as f64 / 1000.0;
            let compute_us = r.server_compute_ns.expect("filtered above") as f64 / 1000.0;
            handler_us - compute_us
        });
        write_stats_row(
            out,
            "server handler overhead = handler \u{2212} A4 (compute)",
            overhead,
            2,
        );
    }
    writeln!(out).unwrap();

    // ── A5 sub-timers ──
    writeln!(out, "### A5 sub-timers ({MEASURED})").unwrap();
    writeln!(out).unwrap();
    writeln!(
        out,
        "| sub-timer | n | mean (us) | p50 (us) | p95 (us) | min (us) | max (us) |"
    )
    .unwrap();
    writeln!(out, "|---|---:|---:|---:|---:|---:|---:|").unwrap();
    let rewind = stats_from(successful.iter().copied(), |r| r.rewind_us as f64);
    let decode = stats_from(successful.iter().copied(), |r| r.decode_us as f64);
    let delta_apply = stats_from(successful.iter().copied(), |r| r.delta_apply_us as f64);
    let scan = stats_from(successful.iter().copied(), |r| r.scan_us as f64);
    write_stats_row(out, "rewind_us", rewind, 2);
    write_stats_row(out, "decode_us", decode, 2);
    write_stats_row(out, "delta_apply_us", delta_apply, 2);
    write_stats_row(out, "scan_us", scan, 2);
    writeln!(out).unwrap();
    let sub_sum_mean = [rewind, decode, delta_apply, scan]
        .iter()
        .map(|s| s.map_or(0.0, |s| s.mean))
        .sum::<f64>();
    writeln!(
        out,
        "Sum of sub-timer means ({DERIVED}): {} us vs. A5 (finish_us) mean: {} us (difference {} us).",
        fmt_dec(sub_sum_mean, 2),
        fmt_dec(a5_mean, 2),
        fmt_dec(sub_sum_mean - a5_mean, 2),
    )
    .unwrap();
    writeln!(out).unwrap();

    // ── Binned by stale_blocks ──
    writeln!(out, "### A1 and A5 binned by `stale_blocks` ({MEASURED})").unwrap();
    writeln!(out).unwrap();
    writeln!(
        out,
        "Bin edges (blocks of staleness): 0\u{2013}99, 100\u{2013}299, 300\u{2013}599, 600\u{2013}899, 900+."
    )
    .unwrap();
    writeln!(out).unwrap();
    writeln!(out, "**A1 (t_total_us) by stale_blocks bin:**").unwrap();
    writeln!(out).unwrap();
    writeln!(out, "| bin | n | mean (us) | p50 (us) | p95 (us) |").unwrap();
    writeln!(out, "|---|---:|---:|---:|---:|").unwrap();
    let a1_bins = binned_stats(successful, |r| r.t_total_us as f64);
    for (i, (label, _, _)) in STALE_BINS.iter().enumerate() {
        write_bin_row(out, label, a1_bins[i]);
    }
    writeln!(out).unwrap();
    writeln!(out, "**A5 (finish_us) by stale_blocks bin:**").unwrap();
    writeln!(out).unwrap();
    writeln!(out, "| bin | n | mean (us) | p50 (us) | p95 (us) |").unwrap();
    writeln!(out, "|---|---:|---:|---:|---:|").unwrap();
    let a5_bins = binned_stats(successful, |r| r.finish_us as f64);
    for (i, (label, _, _)) in STALE_BINS.iter().enumerate() {
        write_bin_row(out, label, a5_bins[i]);
    }
    writeln!(out).unwrap();

    // ── A6 bytes ──
    writeln!(
        out,
        "### A6: wire bytes ({MEASURED}) vs. computed sizes ({COMPUTED})"
    )
    .unwrap();
    writeln!(out).unwrap();
    writeln!(out, "| field | n | mean | min | max |").unwrap();
    writeln!(out, "|---|---:|---:|---:|---:|").unwrap();
    let query_stats = stats_from(successful.iter().copied(), |r| r.query_bytes as f64);
    let response_stats = stats_from(successful.iter().copied(), |r| r.response_bytes as f64);
    let content_len_stats = stats_from(successful.iter().copied(), |r| {
        r.response_content_length as f64
    });
    write_bytes_stats_row(out, "query_bytes", query_stats);
    write_bytes_stats_row(out, "response_bytes", response_stats);
    write_bytes_stats_row(out, "response_content_length", content_len_stats);
    writeln!(out).unwrap();
    let constant = |s: Option<Stats>| s.is_some_and(|s| s.min == s.max);
    if constant(query_stats) && constant(response_stats) && constant(content_len_stats) {
        writeln!(
            out,
            "All three are constant across every successful trial (min == max), as expected for \
             a fixed private-query geometry."
        )
        .unwrap();
    } else {
        writeln!(
            out,
            "At least one of the three varies across trials (min != max) — unexpected for a \
             fixed private-query geometry; see the raw data."
        )
        .unwrap();
    }
    writeln!(out).unwrap();

    let geometry = setup_geometry(&data.setup);
    let sizes = geometry.sizes(Backend::Simple, data.setup.accounts);
    let computed_query_total = sizes.query_per_segment * u64::from(geometry.arity);
    let computed_response_total = sizes.response_per_segment * u64::from(geometry.arity);
    writeln!(
        out,
        "Computed ({COMPUTED}), from `Geometry::sizes` at the `--setup` geometry ({} accounts, \
         arity {}, {} buckets, plaintext_bits {}), assuming the measured `query_bytes`/\
         `response_bytes` cover all `arity` segments of one query:",
        fmt_num(data.setup.accounts),
        geometry.arity,
        fmt_num(u64::from(geometry.num_buckets)),
        geometry.plaintext_bits,
    )
    .unwrap();
    writeln!(out).unwrap();
    writeln!(
        out,
        "| field | measured mean | computed total ({COMPUTED}) |"
    )
    .unwrap();
    writeln!(out, "|---|---:|---:|").unwrap();
    writeln!(
        out,
        "| query bytes | {} | {} |",
        query_stats.map_or("\u{2014}".to_string(), |s| fmt_bytes(s.mean.round() as u64)),
        fmt_bytes(computed_query_total),
    )
    .unwrap();
    writeln!(
        out,
        "| response bytes | {} | {} |",
        response_stats.map_or("\u{2014}".to_string(), |s| fmt_bytes(s.mean.round() as u64)),
        fmt_bytes(computed_response_total),
    )
    .unwrap();
    writeln!(out).unwrap();

    // ── found / absent_probe ──
    let found_count = successful.iter().filter(|r| r.found).count();
    let absent_probe_count = successful.iter().filter(|r| r.absent_probe).count();
    writeln!(
        out,
        "`found` ({MEASURED}): {found_count}/{} successful trials. `absent_probe` ({MEASURED}): \
         {absent_probe_count}/{} successful trials.",
        successful.len(),
        successful.len(),
    )
    .unwrap();
    writeln!(out).unwrap();

    // ── provider match summary ──
    let matched: Vec<&&TrialRow> = successful
        .iter()
        .filter(|r| r.provider_match == Some(true))
        .collect();
    let mismatched_count = successful
        .iter()
        .filter(|r| r.provider_match == Some(false))
        .count();
    let unavailable_count = successful
        .iter()
        .filter(|r| r.provider_match.is_none())
        .count();
    let all_blocks: Vec<u64> = successful.iter().map(|r| r.at_block).collect();
    let overall_range = all_blocks.iter().min().zip(all_blocks.iter().max());
    writeln!(
        out,
        "Provider match summary ({MEASURED}): matched {}, mismatched {mismatched_count}, \
         unavailable {unavailable_count}{}.",
        matched.len(),
        overall_range.map_or(String::new(), |(lo, hi)| format!(
            " (block range {}\u{2013}{} across all successful trials)",
            fmt_num(*lo),
            fmt_num(*hi)
        )),
    )
    .unwrap();
    writeln!(out).unwrap();

    let matched_blocks: BTreeSet<u64> = matched.iter().map(|r| r.at_block).collect();
    if matched.is_empty() {
        writeln!(
            out,
            "**Correctness evidence:** no matched private queries in this dataset."
        )
        .unwrap();
    } else {
        writeln!(
            out,
            "**Correctness evidence:** {} private queries returned byte-exact balances against \
             an independent provider across {} blocks ({}\u{2013}{}).",
            matched.len(),
            matched_blocks.len(),
            fmt_num(*matched_blocks.iter().next().expect("non-empty")),
            fmt_num(*matched_blocks.iter().next_back().expect("non-empty")),
        )
        .unwrap();
    }
    writeln!(out).unwrap();

    let attempts_gt1 = successful.iter().filter(|r| r.attempts > 1).count();
    writeln!(
        out,
        "`attempts > 1` ({MEASURED}): {attempts_gt1}/{} successful trials.",
        successful.len()
    )
    .unwrap();
    writeln!(out).unwrap();
}

fn write_bin_row(out: &mut String, label: &str, stats: Option<Stats>) {
    match stats {
        Some(s) => writeln!(
            out,
            "| {label} | {} | {} | {} | {} |",
            s.n,
            fmt_dec(s.mean, 2),
            fmt_dec(s.p50, 2),
            fmt_dec(s.p95, 2),
        )
        .unwrap(),
        None => writeln!(out, "| {label} | 0 | \u{2014} | \u{2014} | \u{2014} |").unwrap(),
    }
}

fn render_section_b(out: &mut String, data: &ReportData) {
    writeln!(out, "## B. One block").unwrap();
    writeln!(out).unwrap();

    writeln!(out, "### B7: per-block mutation counts ({MEASURED})").unwrap();
    writeln!(out).unwrap();
    writeln!(out, "| metric | n | mean | p50 | p95 | min | max | total |").unwrap();
    writeln!(out, "|---|---:|---:|---:|---:|---:|---:|---:|").unwrap();
    let col =
        |f: fn(&ServerBlockRow) -> u64| -> Vec<u64> { data.server_blocks.iter().map(f).collect() };
    write_count_stats_row_with_total(out, "inserts", &col(|r| r.inserts));
    write_count_stats_row_with_total(out, "updates", &col(|r| r.updates));
    write_count_stats_row_with_total(out, "deletes", &col(|r| r.deletes));
    write_count_stats_row_with_total(out, "noop_deletes", &col(|r| r.noop_deletes));
    write_count_stats_row_with_total(out, "changes", &col(|r| r.changes));
    write_count_stats_row_with_total(out, "credits", &col(|r| r.credits));
    write_count_stats_row_with_total(out, "touched_cells", &col(|r| r.touched_cells));
    writeln!(out).unwrap();

    writeln!(out, "### B8: apply_ms by interference subset ({MEASURED})").unwrap();
    writeln!(out).unwrap();
    writeln!(
        out,
        "| subset | n | mean (ms) | p50 (ms) | p95 (ms) | min (ms) | max (ms) |"
    )
    .unwrap();
    writeln!(out, "|---|---:|---:|---:|---:|---:|---:|").unwrap();
    let quiet: Vec<&ServerBlockRow> = data
        .server_blocks
        .iter()
        .filter(|r| r.answers_since_prev_block == Some(0))
        .collect();
    let probe_adjacent: Vec<&ServerBlockRow> = data
        .server_blocks
        .iter()
        .filter(|r| matches!(r.answers_since_prev_block, Some(n) if n > 0))
        .collect();
    let unknown_interference = data
        .server_blocks
        .iter()
        .filter(|r| r.answers_since_prev_block.is_none())
        .count();
    write_stats_row(
        out,
        "all blocks",
        stats_from(data.server_blocks.iter(), |r| r.apply_ms),
        4,
    );
    write_stats_row(
        out,
        "quiet (answers_since_prev_block == 0)",
        stats_from(quiet.iter().copied(), |r| r.apply_ms),
        4,
    );
    write_stats_row(
        out,
        "probe-adjacent (answers_since_prev_block > 0)",
        stats_from(probe_adjacent.iter().copied(), |r| r.apply_ms),
        4,
    );
    writeln!(out).unwrap();
    writeln!(
        out,
        "{unknown_interference} row(s) have an empty `answers_since_prev_block` (the first \
         block a follow-loop run applies, whose window is undefined) and belong to neither \
         subset above."
    )
    .unwrap();
    writeln!(out).unwrap();

    writeln!(out, "### B8: stage breakdown ({MEASURED}, {DERIVED})").unwrap();
    writeln!(out).unwrap();
    writeln!(
        out,
        "| stage | n | mean (ms) | p50 (ms) | p95 (ms) | min (ms) | max (ms) |"
    )
    .unwrap();
    writeln!(out, "|---|---:|---:|---:|---:|---:|---:|").unwrap();
    write_stats_row(
        out,
        "store_ms",
        stats_from(data.server_blocks.iter(), |r| r.store_ms),
        4,
    );
    write_stats_row(
        out,
        "fold_ms",
        stats_from(data.server_blocks.iter(), |r| r.fold_ms),
        4,
    );
    write_stats_row(
        out,
        "patch_ms",
        stats_from(data.server_blocks.iter(), |r| r.patch_ms),
        4,
    );
    write_stats_row(
        out,
        &format!("residual_ms = apply_ms \u{2212} (store+fold+patch) ({DERIVED})"),
        stats_from(data.server_blocks.iter(), |r| {
            r.apply_ms - (r.store_ms + r.fold_ms + r.patch_ms)
        }),
        4,
    );
    write_stats_row(
        out,
        "lock_wait_ms",
        stats_from(data.server_blocks.iter(), |r| r.lock_wait_ms),
        4,
    );
    writeln!(out).unwrap();

    writeln!(
        out,
        "### B9: delta bytes, server vs. client ({MEASURED}, {DERIVED})"
    )
    .unwrap();
    writeln!(out).unwrap();
    writeln!(out, "| source | n | mean | p50 | p95 | min | max |").unwrap();
    writeln!(out, "|---|---:|---:|---:|---:|---:|---:|").unwrap();
    let server_delta_stats = stats_from(data.server_blocks.iter(), |r| r.delta_bytes as f64);
    let single_block = single_block_client_rows(&data.client_blocks);
    let client_wire_stats = stats_from(single_block.iter().copied(), |r| r.wire_bytes as f64);
    write_byte_metric_row(out, "server delta_bytes", server_delta_stats);
    write_byte_metric_row(
        out,
        "client wire_bytes (single-block fetches)",
        client_wire_stats,
    );
    writeln!(out).unwrap();
    match (server_delta_stats, client_wire_stats) {
        (Some(s), Some(c)) => writeln!(
            out,
            "{DERIVED}: mean(client wire_bytes) \u{2212} mean(server delta_bytes) = {} bytes.",
            fmt_dec(c.mean - s.mean, 1),
        )
        .unwrap(),
        _ => writeln!(out, "{DERIVED}: not enough data to compare.").unwrap(),
    }
    writeln!(out).unwrap();

    writeln!(
        out,
        "### B10: client ingest/decode, single-block fetches ({MEASURED})"
    )
    .unwrap();
    writeln!(out).unwrap();
    writeln!(
        out,
        "| metric | n | mean (us) | p50 (us) | p95 (us) | min (us) | max (us) |"
    )
    .unwrap();
    writeln!(out, "|---|---:|---:|---:|---:|---:|---:|").unwrap();
    write_stats_row(
        out,
        "ingest_us",
        stats_from(single_block.iter().copied(), |r| r.ingest_us as f64),
        2,
    );
    write_stats_row(
        out,
        "decode_us",
        stats_from(single_block.iter().copied(), |r| r.decode_us as f64),
        2,
    );
    writeln!(out).unwrap();
    writeln!(
        out,
        "{} client-block row(s) coalesced more than one block (`blocks_in_fetch != 1`) and are \
         excluded from B9/B10's client-side statistics above.",
        coalesced_client_row_count(&data.client_blocks)
    )
    .unwrap();
    writeln!(out).unwrap();

    let server_range = block_range(data.server_blocks.iter().map(|r| r.block));
    let client_range = block_range(data.client_blocks.iter().map(|r| r.block));
    let server_set: BTreeSet<u64> = data.server_blocks.iter().map(|r| r.block).collect();
    let client_set: BTreeSet<u64> = data.client_blocks.iter().map(|r| r.block).collect();
    let overlap = server_set.intersection(&client_set).count();
    writeln!(
        out,
        "Server block range ({MEASURED}): {} ({} blocks). Client block range ({MEASURED}): {} \
         ({} blocks). Overlap: {overlap} block(s) observed by both.",
        fmt_range(server_range),
        data.server_blocks.len(),
        fmt_range(client_range),
        data.client_blocks.len(),
    )
    .unwrap();
    writeln!(out).unwrap();
}

fn write_byte_metric_row(out: &mut String, label: &str, stats: Option<Stats>) {
    match stats {
        Some(s) => writeln!(
            out,
            "| {label} | {} | {} | {} | {} | {} | {} |",
            s.n,
            fmt_bytes(s.mean.round() as u64),
            fmt_bytes(s.p50.round() as u64),
            fmt_bytes(s.p95.round() as u64),
            fmt_bytes(s.min.round() as u64),
            fmt_bytes(s.max.round() as u64),
        )
        .unwrap(),
        None => writeln!(
            out,
            "| {label} | 0 | \u{2014} | \u{2014} | \u{2014} | \u{2014} | \u{2014} |"
        )
        .unwrap(),
    }
}

fn block_range(blocks: impl Iterator<Item = u64>) -> Option<(u64, u64)> {
    let mut min = None;
    let mut max = None;
    for b in blocks {
        min = Some(min.map_or(b, |m: u64| m.min(b)));
        max = Some(max.map_or(b, |m: u64| m.max(b)));
    }
    min.zip(max)
}

fn fmt_range(range: Option<(u64, u64)>) -> String {
    match range {
        Some((lo, hi)) => format!("{}\u{2013}{}", fmt_num(lo), fmt_num(hi)),
        None => "(no rows)".to_string(),
    }
}

fn render_section_c(out: &mut String, data: &ReportData) {
    writeln!(out, "## C. One time").unwrap();
    writeln!(out).unwrap();

    writeln!(out, "### C11: scale and sizes ({MEASURED}, {COMPUTED})").unwrap();
    writeln!(out).unwrap();
    let geometry = setup_geometry(&data.setup);
    let sizes = geometry.sizes(Backend::Simple, data.setup.accounts);
    let computed_hint_total = sizes.hint_per_segment * u64::from(geometry.arity);
    let computed_a_total = sizes.a_per_segment * u64::from(geometry.arity);
    writeln!(
        out,
        "| field | measured (--setup) | computed (`Geometry::sizes`) |"
    )
    .unwrap();
    writeln!(out, "|---|---:|---:|").unwrap();
    writeln!(
        out,
        "| accounts | {} | \u{2014} |",
        fmt_num(data.setup.accounts)
    )
    .unwrap();
    writeln!(
        out,
        "| DB bytes | {} | {} |",
        fmt_bytes(data.setup.cells_bytes),
        fmt_bytes(sizes.server_db),
    )
    .unwrap();
    writeln!(
        out,
        "| hint bytes | {} | {} |",
        fmt_bytes(data.setup.hint_bytes),
        fmt_bytes(computed_hint_total),
    )
    .unwrap();
    writeln!(
        out,
        "| A bytes | \u{2014} (not reported by `time-setup`) | {} |",
        fmt_bytes(computed_a_total),
    )
    .unwrap();
    writeln!(out).unwrap();
    if data.setup.lwe_dim != SimpleParams::DEFAULT_LWE_DIM {
        writeln!(
            out,
            "**Warning:** measured `lwe_dim` ({}) differs from this workspace's fixed \
             `SimpleParams::DEFAULT_LWE_DIM` ({}), which `Geometry::sizes` always computes \
             against (it takes no `lwe_dim` parameter of its own) — the computed sizes above \
             assume the default and may not describe this campaign.",
            data.setup.lwe_dim,
            SimpleParams::DEFAULT_LWE_DIM,
        )
        .unwrap();
        writeln!(out).unwrap();
    }

    writeln!(out, "### C12: setup download and client RSS ({MEASURED})").unwrap();
    writeln!(out).unwrap();
    match &data.setup_download {
        Some(sd) => {
            writeln!(out, "| field | value |").unwrap();
            writeln!(out, "|---|---:|").unwrap();
            writeln!(out, "| setup_bytes | {} |", fmt_bytes(sd.setup_bytes)).unwrap();
            writeln!(out, "| content_length | {} |", fmt_bytes(sd.content_length)).unwrap();
            writeln!(out, "| wall_seconds | {} |", fmt_dec(sd.wall_seconds, 3)).unwrap();
            writeln!(out, "| pinned_block | {} |", fmt_num(sd.pinned_block)).unwrap();
        }
        None => {
            writeln!(
                out,
                "No `--setup-download` given; `/setup` download bytes not reported (they are not \
                 in the `--trials`/`--server-blocks`/`--client-blocks`/`--setup` files)."
            )
            .unwrap();
        }
    }
    writeln!(out).unwrap();
    let rss_values: Vec<f64> = data
        .trials
        .iter()
        .filter_map(|r| r.client_rss_bytes)
        .map(|v| v as f64)
        .collect();
    writeln!(out, "Client RSS ({MEASURED}, sampled `client_rss_bytes`):").unwrap();
    writeln!(out).unwrap();
    writeln!(out, "| n | mean | min | max |").unwrap();
    writeln!(out, "|---:|---:|---:|---:|").unwrap();
    write_bytes_stats_row_no_label(out, compute_stats(&rss_values));
    writeln!(out).unwrap();

    writeln!(out, "### C13: setup measurement ({MEASURED})").unwrap();
    writeln!(out).unwrap();
    writeln!(out, "| field | value |").unwrap();
    writeln!(out, "|---|---:|").unwrap();
    writeln!(
        out,
        "| setup_seconds | {} |",
        fmt_dec(data.setup.setup_seconds, 3)
    )
    .unwrap();
    writeln!(
        out,
        "| persisted_hints_exact_match (invariant check) | {} |",
        data.setup.persisted_hints_exact_match
    )
    .unwrap();
    writeln!(out, "| state_block | {} |", fmt_num(data.setup.state_block)).unwrap();
    writeln!(out, "| lwe_dim | {} |", data.setup.lwe_dim).unwrap();
    writeln!(out, "| rayon_threads | {} |", data.setup.rayon_threads).unwrap();
    writeln!(out).unwrap();
}

fn write_bytes_stats_row_no_label(out: &mut String, stats: Option<Stats>) {
    match stats {
        Some(s) => writeln!(
            out,
            "| {} | {} | {} | {} |",
            s.n,
            fmt_bytes(s.mean.round() as u64),
            fmt_bytes(s.min.round() as u64),
            fmt_bytes(s.max.round() as u64),
        )
        .unwrap(),
        None => writeln!(out, "| 0 | \u{2014} | \u{2014} | \u{2014} |").unwrap(),
    }
}

/// Cap on how many individual data-quality findings (e.g.
/// `provider_match == 0` rows) §D lists by name before summarizing the
/// rest as a count, so one badly-behaved campaign cannot blow up the
/// report's size.
const MAX_LISTED_FINDINGS: usize = 20;

fn render_section_d(
    out: &mut String,
    data: &ReportData,
    successful: &[&TrialRow],
    errored_count: usize,
) {
    writeln!(out, "## D. Interference and data-quality notes").unwrap();
    writeln!(out).unwrap();

    writeln!(
        out,
        "- **Errored trials:** {errored_count}/{}.",
        data.trials.len()
    )
    .unwrap();
    if errored_count > 0 {
        let mut counts: Vec<(String, usize)> = Vec::new();
        for r in &data.trials {
            if let Some(msg) = &r.error {
                match counts.iter_mut().find(|(m, _)| m == msg) {
                    Some((_, c)) => *c += 1,
                    None => counts.push((msg.clone(), 1)),
                }
            }
        }
        counts.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        for (msg, count) in counts.iter().take(MAX_LISTED_FINDINGS) {
            writeln!(out, "  - {count}x: {msg}").unwrap();
        }
        if counts.len() > MAX_LISTED_FINDINGS {
            writeln!(
                out,
                "  - (+{} more distinct error message(s))",
                counts.len() - MAX_LISTED_FINDINGS
            )
            .unwrap();
        }
    }

    writeln!(
        out,
        "- **Coalesced client-block fetch rows excluded from B9/B10:** {}.",
        coalesced_client_row_count(&data.client_blocks)
    )
    .unwrap();

    let probe_adjacent_count = data
        .server_blocks
        .iter()
        .filter(|r| matches!(r.answers_since_prev_block, Some(n) if n > 0))
        .count();
    let unknown_interference = data
        .server_blocks
        .iter()
        .filter(|r| r.answers_since_prev_block.is_none())
        .count();
    writeln!(
        out,
        "- **Blocks applied while probe traffic was active (`answers_since_prev_block > 0`):** \
         {probe_adjacent_count}/{}{}.",
        data.server_blocks.len(),
        if unknown_interference == 0 {
            String::new()
        } else {
            format!(
                " ({unknown_interference} row(s) carry an empty `answers_since_prev_block` \
                 \u{2014} the first block of a follow-loop run, whose window is undefined \u{2014} \
                 and are counted in neither the quiet nor the probe-adjacent subset)"
            )
        }
    )
    .unwrap();

    let (max_violation, worst_trial) = max_budget_violation(successful);
    writeln!(
        out,
        "- **Max budget identity violation:** {} us{}.",
        max_violation.unsigned_abs(),
        worst_trial.map_or(String::new(), |t| format!(" (worst at trial {t})")),
    )
    .unwrap();

    let mismatches: Vec<(u64, u64)> = successful
        .iter()
        .filter(|r| r.provider_match == Some(false))
        .map(|r| (r.at_block, r.trial))
        .collect();
    writeln!(
        out,
        "- **`provider_match == 0` (independent-provider mismatch):** {} row(s).",
        mismatches.len()
    )
    .unwrap();
    for (block, trial) in mismatches.iter().take(MAX_LISTED_FINDINGS) {
        writeln!(out, "  - block {}, trial {trial}", fmt_num(*block)).unwrap();
    }
    if mismatches.len() > MAX_LISTED_FINDINGS {
        writeln!(
            out,
            "  - (+{} more)",
            mismatches.len() - MAX_LISTED_FINDINGS
        )
        .unwrap();
    }
    writeln!(out).unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Nearest-rank percentiles: n=1, n=2, n=20 hand-computed ─────────

    #[test]
    fn nearest_rank_n1() {
        let s = compute_stats(&[42.0]).expect("non-empty");
        assert_eq!(s.n, 1);
        assert_eq!(s.mean, 42.0);
        // rank = ceil(0.5*1) = 1 -> index 0; rank = ceil(0.95*1) = 1 -> index 0.
        assert_eq!(s.p50, 42.0);
        assert_eq!(s.p95, 42.0);
        assert_eq!(s.min, 42.0);
        assert_eq!(s.max, 42.0);
    }

    #[test]
    fn nearest_rank_n2() {
        // sorted [10, 20]: p50 rank = ceil(0.5*2) = 1 -> index 0 -> 10.
        // p95 rank = ceil(0.95*2) = ceil(1.9) = 2 -> index 1 -> 20.
        let s = compute_stats(&[20.0, 10.0]).expect("non-empty");
        assert_eq!(s.n, 2);
        assert_eq!(s.mean, 15.0);
        assert_eq!(s.p50, 10.0);
        assert_eq!(s.p95, 20.0);
        assert_eq!(s.min, 10.0);
        assert_eq!(s.max, 20.0);
    }

    #[test]
    fn nearest_rank_n20() {
        // 1..=20, shuffled input order (sorting must not depend on input order).
        let values: Vec<f64> = vec![
            5.0, 20.0, 1.0, 14.0, 3.0, 18.0, 7.0, 2.0, 19.0, 6.0, 11.0, 9.0, 16.0, 4.0, 13.0, 8.0,
            17.0, 10.0, 15.0, 12.0,
        ];
        assert_eq!(values.len(), 20);
        let s = compute_stats(&values).expect("non-empty");
        assert_eq!(s.n, 20);
        // sorted = [1..=20]; p50 rank = ceil(0.5*20) = 10 -> index 9 -> value 10.
        // p95 rank = ceil(0.95*20) = ceil(19.0) = 19 -> index 18 -> value 19.
        assert_eq!(s.p50, 10.0);
        assert_eq!(s.p95, 19.0);
        assert_eq!(s.min, 1.0);
        assert_eq!(s.max, 20.0);
        assert_eq!(s.mean, 10.5);
    }

    #[test]
    fn compute_stats_empty_is_none() {
        assert!(compute_stats(&[]).is_none());
    }

    // ── Budget identity assertion ───────────────────────────────────────

    /// Builds one syntactically valid `--trials` row from named overrides,
    /// leaving every other column at a fixed, self-consistent default —
    /// so each test only spells out the fields it cares about.
    fn trial_csv_row(overrides: &[(&str, &str)]) -> String {
        let mut fields: Vec<(&str, String)> = vec![
            ("batch", "1".into()),
            ("trial", "1".into()),
            ("started_at_unix_ms", "1000".into()),
            ("absent_probe", "0".into()),
            ("t_total_us", "500".into()),
            ("build_us", "100".into()),
            ("head_wire_us", "50".into()),
            ("sync_wire_us", "60".into()),
            ("answer_wire_us", "70".into()),
            ("setup_wire_us", "0".into()),
            ("finish_us", "200".into()),
            ("residual_us", "20".into()),
            ("rewind_us", "50".into()),
            ("decode_us", "50".into()),
            ("delta_apply_us", "50".into()),
            ("scan_us", "50".into()),
            ("server_compute_ns", "100000".into()),
            ("server_handler_ns", "150000".into()),
            ("query_bytes", "1000".into()),
            ("response_bytes", "2000".into()),
            ("response_content_length", "2000".into()),
            ("at_block", "100".into()),
            ("pinned_block", "90".into()),
            ("stale_blocks", "10".into()),
            ("delta_cells", "5".into()),
            ("found", "1".into()),
            ("provider_match", "1".into()),
            ("provider_error", "".into()),
            ("provider_rtt_us", "500".into()),
            ("client_rss_bytes", "123456".into()),
            ("attempts", "1".into()),
            ("error", "".into()),
        ];
        for (name, value) in overrides {
            let slot = fields
                .iter_mut()
                .find(|(n, _)| n == name)
                .unwrap_or_else(|| panic!("trial_csv_row: unknown column {name:?}"));
            slot.1 = (*value).to_string();
        }
        fields
            .into_iter()
            .map(|(_, v)| v)
            .collect::<Vec<_>>()
            .join(",")
    }

    fn trials_csv_with_rows(rows: &[String]) -> String {
        let mut out = TRIALS_COLUMNS.join(",");
        out.push('\n');
        for row in rows {
            out.push_str(row);
            out.push('\n');
        }
        out
    }

    #[test]
    fn budget_violation_is_zero_for_a_consistent_row() {
        // t_total_us = 500 = build(100) + head(50) + sync(60) + answer(70) + finish(200) + residual(20).
        let csv = trials_csv_with_rows(&[trial_csv_row(&[])]);
        let rows = parse_trials_csv(&csv).expect("valid fixture");
        let refs: Vec<&TrialRow> = rows.iter().collect();
        let (violation, worst) = max_budget_violation(&refs);
        assert_eq!(violation, 0);
        assert!(worst.is_none(), "no violation means no offending trial");
    }

    #[test]
    fn budget_violation_detects_a_violating_row() {
        // Same shape as the consistent row, but t_total_us is wrong by 7 us.
        let bad = trial_csv_row(&[("trial", "42"), ("t_total_us", "507")]);
        let good = trial_csv_row(&[("trial", "1")]);
        let csv = trials_csv_with_rows(&[good, bad]);
        let rows = parse_trials_csv(&csv).expect("valid fixture");
        let refs: Vec<&TrialRow> = rows.iter().collect();
        let (violation, worst) = max_budget_violation(&refs);
        assert_eq!(violation, 7);
        assert_eq!(
            worst,
            Some(42),
            "must identify the violating row's trial id"
        );
    }

    // ── Staleness bins ──────────────────────────────────────────────────

    #[test]
    fn stale_bin_index_edges() {
        assert_eq!(stale_bin_index(-5), 0, "negative treated as not-stale");
        assert_eq!(stale_bin_index(0), 0);
        assert_eq!(stale_bin_index(99), 0);
        assert_eq!(stale_bin_index(100), 1);
        assert_eq!(stale_bin_index(299), 1);
        assert_eq!(stale_bin_index(300), 2);
        assert_eq!(stale_bin_index(599), 2);
        assert_eq!(stale_bin_index(600), 3);
        assert_eq!(stale_bin_index(899), 3);
        assert_eq!(stale_bin_index(900), 4);
        assert_eq!(stale_bin_index(1_000_000), 4);
    }

    // ── Single-block client-fetch filter ────────────────────────────────

    const CLIENT_BLOCKS_HEADER: &str =
        "block,received_at_unix_ms,wire_bytes,decode_us,ingest_us,cells_in_block,delta_cells_total,fetch_wire_us,blocks_in_fetch";

    #[test]
    fn single_block_filter_excludes_coalesced_rows() {
        let csv = format!(
            "{CLIENT_BLOCKS_HEADER}\n\
             100,1000,5000,40,60,10,10,45,1\n\
             105,2000,20000,100,150,50,50,110,5\n\
             300,3000,6000,42,61,11,11,46,1\n"
        );
        let rows = parse_client_blocks_csv(&csv).expect("valid fixture");
        assert_eq!(rows.len(), 3);
        let single = single_block_client_rows(&rows);
        assert_eq!(single.len(), 2);
        assert!(single.iter().all(|r| r.blocks_in_fetch == 1));
        assert_eq!(coalesced_client_row_count(&rows), 1);
    }

    // ── Missing-column failure ──────────────────────────────────────────

    #[test]
    fn missing_column_fails_loudly() {
        let csv = "block,received_at_unix_ms,wire_bytes,decode_us\n100,1000,5000,40\n";
        let err = parse_client_blocks_csv(csv).expect_err("header is missing 5 columns");
        assert!(
            err.contains("ingest_us") && err.contains("--client-blocks"),
            "error must name the file and the missing column: {err}"
        );
    }

    #[test]
    fn ragged_row_fails_loudly_rather_than_misaligning() {
        let csv = format!("{CLIENT_BLOCKS_HEADER}\n100,1000,5000,40,60,10,10,45\n");
        let err = parse_client_blocks_csv(&csv).expect_err("row has one fewer field than header");
        assert!(
            err.contains("row 2"),
            "error must name the offending row: {err}"
        );
    }

    // ── Label strings ────────────────────────────────────────────────────

    #[test]
    fn label_constants_are_the_expected_words() {
        assert_eq!(MEASURED, "measured");
        assert_eq!(COMPUTED, "computed");
        assert_eq!(DERIVED, "derived");
    }

    // ── Golden fixture shared by the rendering tests below ──────────────

    const SERVER_BLOCKS_HEADER: &str = "block,applied_at_unix_ms,changes,credits,inserts,updates,deletes,noop_deletes,touched_cells,store_ms,fold_ms,patch_ms,apply_ms,lock_wait_ms,delta_bytes,answers_since_prev_block,answer_compute_ms_since_prev_block,feed_fetch_ms,finalized_block";

    fn golden_report_data() -> ReportData {
        let trials_csv = trials_csv_with_rows(&[
            trial_csv_row(&[]),
            trial_csv_row(&[
                ("trial", "2"),
                ("t_total_us", "540"),
                ("build_us", "110"),
                ("head_wire_us", "55"),
                ("sync_wire_us", "65"),
                ("answer_wire_us", "75"),
                ("finish_us", "210"),
                ("residual_us", "25"),
                ("rewind_us", "55"),
                ("decode_us", "55"),
                ("delta_apply_us", "50"),
                ("scan_us", "50"),
                ("server_compute_ns", ""),
                ("server_handler_ns", ""),
                ("at_block", "300"),
                ("pinned_block", "100"),
                ("stale_blocks", "200"),
                ("found", "0"),
                ("absent_probe", "1"),
                ("provider_match", "0"),
                ("provider_error", "Timeout"),
                ("attempts", "2"),
            ]),
            trial_csv_row(&[
                ("trial", "3"),
                ("batch", "2"),
                ("t_total_us", "0"),
                ("build_us", "0"),
                ("head_wire_us", "0"),
                ("sync_wire_us", "0"),
                ("answer_wire_us", "0"),
                ("finish_us", "0"),
                ("residual_us", "0"),
                ("rewind_us", "0"),
                ("decode_us", "0"),
                ("delta_apply_us", "0"),
                ("scan_us", "0"),
                ("server_compute_ns", ""),
                ("server_handler_ns", ""),
                ("at_block", "50"),
                ("pinned_block", "50"),
                ("stale_blocks", "0"),
                ("found", "0"),
                ("provider_match", ""),
                ("client_rss_bytes", ""),
                ("error", "DecodeFailed"),
            ]),
        ]);
        let client_blocks_csv = format!(
            "{CLIENT_BLOCKS_HEADER}\n\
             100,1000,5000,40,60,10,10,45,1\n\
             105,2000,20000,100,150,50,50,110,5\n\
             300,3000,6000,42,61,11,11,46,1\n"
        );
        let server_blocks_csv = format!(
            "{SERVER_BLOCKS_HEADER}\n\
             100,1000,10,2,3,4,1,0,50,1.0,0.5,0.3,1.9,0.05,5000,0,0,2.0,99\n\
             300,3000,8,1,2,3,2,1,40,0.9,0.4,0.2,1.6,0.02,6000,2,5.0,1.8,299\n"
        );
        // A geometry invented for this fixture, not any repo citation —
        // deliberately not one of `xtask::bench`'s own scales (100_000 /
        // 1_000_000 / 9_437_184), so nothing here can be mistaken for a
        // pinned real measurement. `cells_bytes`/`hint_bytes` are this
        // module's own `setup_geometry`/`Geometry::sizes` arithmetic
        // worked by hand from this geometry (fingerprint_bits 32,
        // value_bits 144, lwe_dim 1275 — this workspace's fixed
        // constants): cells_per_slot = ceil((32+144)/9) = 20, row_width =
        // 4*20 = 80, segment_rows = 16384/2 = 8192; SimplePIR reshape
        // k=10, R=820, C=800; hint/segment = 1275*800*4 = 4,080,000 (×2
        // segments = 8,160,000); server_db = 16384*4*20*4 = 5,242,880 —
        // exercising the exact-match ("everything checks out") path
        // without citing any published number.
        let setup_json = r#"{
            "accounts": 64000,
            "buckets": 16384,
            "cells_bytes": 5242880,
            "hint_bytes": 8160000,
            "arity": 2,
            "bucket_size": 4,
            "lwe_dim": 1275,
            "plaintext_bits": 9,
            "setup_seconds": 12.5,
            "persisted_hints_exact_match": true,
            "state_block": 300,
            "rayon_threads": 8
        }"#;
        let setup_download_json = r#"{
            "setup_bytes": 8160250,
            "content_length": 8160250,
            "wall_seconds": 1.8,
            "pinned_block": 100
        }"#;
        let provenance_raw = "# test provenance\ncommit = \"abc123\"\ngeometry = \"arity2 bucket4\"\nhost = \"test-host\"\n";

        ReportData::parse(
            &trials_csv,
            &client_blocks_csv,
            &server_blocks_csv,
            setup_json,
            Some(setup_download_json),
            provenance_raw,
            false,
        )
        .expect("golden fixture must parse")
    }

    #[test]
    fn setup_geometry_reproduces_the_fixture_arithmetic_by_hand() {
        // Same arithmetic as `golden_report_data`'s own doc comment,
        // worked independently here — a regression check on
        // `setup_geometry`/`Geometry::sizes`'s wiring, not a citation of
        // any published number.
        let data = golden_report_data();
        let geometry = setup_geometry(&data.setup);
        let sizes = geometry.sizes(Backend::Simple, data.setup.accounts);
        assert_eq!(sizes.server_db, 5_242_880);
        assert_eq!(sizes.hint_per_segment, 4_080_000);
        assert_eq!(
            sizes.hint_per_segment * u64::from(geometry.arity),
            8_160_000
        );
    }

    #[test]
    fn c11_warns_when_measured_lwe_dim_differs_from_the_default() {
        // `Geometry::sizes` always computes against
        // `SimpleParams::DEFAULT_LWE_DIM` — it takes no `lwe_dim`
        // parameter of its own — so a campaign whose measured `lwe_dim`
        // differs must be flagged, not silently computed against the
        // wrong dimension.
        let trials_csv = format!("{}\n", TRIALS_COLUMNS.join(","));
        let client_blocks_csv = format!("{CLIENT_BLOCKS_HEADER}\n");
        let server_blocks_csv = format!("{SERVER_BLOCKS_HEADER}\n");
        // An arbitrary, invented small geometry — only `lwe_dim` (999, an
        // obviously-not-real dimension) matters to this test; the other
        // fields just need to be internally valid, not realistic.
        let setup_json = r#"{
            "accounts": 1000,
            "buckets": 1024,
            "cells_bytes": 111111,
            "hint_bytes": 222222,
            "arity": 2,
            "bucket_size": 4,
            "lwe_dim": 999,
            "plaintext_bits": 8,
            "setup_seconds": 3.0,
            "persisted_hints_exact_match": true,
            "state_block": 300,
            "rayon_threads": 8
        }"#;
        let data = ReportData::parse(
            &trials_csv,
            &client_blocks_csv,
            &server_blocks_csv,
            setup_json,
            None,
            "",
            false,
        )
        .expect("valid minimal fixture");
        let markdown = render_markdown(&data);
        assert!(markdown.contains("**Warning:** measured `lwe_dim` (999)"));
        assert!(markdown.contains("| lwe_dim | 999 |"));
    }

    #[test]
    fn c11_no_warning_when_measured_lwe_dim_matches_the_default() {
        let data = golden_report_data();
        let markdown = render_markdown(&data);
        assert!(!markdown.contains("**Warning:** measured `lwe_dim`"));
        assert!(markdown.contains(&format!("| lwe_dim | {} |", SimpleParams::DEFAULT_LWE_DIM)));
    }

    #[test]
    fn golden_markdown_contains_every_expected_section_heading() {
        let data = golden_report_data();
        let markdown = render_markdown(&data);

        let expected_headings = [
            "# RisePIR measurement campaign report",
            "## 0. Provenance and method",
            "## A. One private query",
            "### A1\u{2013}A6: per-query timing (measured)",
            "### Budget identity (derived)",
            "### A3, A4, and server handler overhead (measured, derived)",
            "### A5 sub-timers (measured)",
            "### A1 and A5 binned by `stale_blocks` (measured)",
            "### A6: wire bytes (measured) vs. computed sizes (computed)",
            "**Correctness evidence:**",
            "## B. One block",
            "### B7: per-block mutation counts (measured)",
            "### B8: apply_ms by interference subset (measured)",
            "### B8: stage breakdown (measured, derived)",
            "### B9: delta bytes, server vs. client (measured, derived)",
            "### B10: client ingest/decode, single-block fetches (measured)",
            "## C. One time",
            "### C11: scale and sizes (measured, computed)",
            "### C12: setup download and client RSS (measured)",
            "### C13: setup measurement (measured)",
            "## D. Interference and data-quality notes",
        ];
        for heading in expected_headings {
            assert!(
                markdown.contains(heading),
                "missing expected heading {heading:?} in:\n{markdown}"
            );
        }
    }

    // ── B9 byte-row cell count (regression: `max` was silently dropped) ──

    #[test]
    fn write_byte_metric_row_emits_a_cell_for_every_header_column() {
        let header = "| source | n | mean | p50 | p95 | min | max |";
        let header_cells = header.matches('|').count();

        let mut some_out = String::new();
        write_byte_metric_row(&mut some_out, "label", compute_stats(&[10.0, 20.0, 30.0]));
        assert_eq!(
            some_out.trim_end().matches('|').count(),
            header_cells,
            "Some(stats) row must match the header's cell count: {some_out:?}"
        );

        let mut none_out = String::new();
        write_byte_metric_row(&mut none_out, "label", None);
        assert_eq!(
            none_out.trim_end().matches('|').count(),
            header_cells,
            "None row must match the header's cell count: {none_out:?}"
        );
    }

    #[test]
    fn b9_rows_have_as_many_cells_as_their_header() {
        let data = golden_report_data();
        let markdown = render_markdown(&data);
        let header = markdown
            .lines()
            .find(|l| l.starts_with("| source |"))
            .expect("B9 header present");
        let header_cells = header.matches('|').count();
        let server_row = markdown
            .lines()
            .find(|l| l.starts_with("| server delta_bytes |"))
            .expect("B9 server row present");
        let client_row = markdown
            .lines()
            .find(|l| l.starts_with("| client wire_bytes"))
            .expect("B9 client row present");
        assert_eq!(server_row.matches('|').count(), header_cells);
        assert_eq!(client_row.matches('|').count(), header_cells);
    }

    // ── A4 (measured), gated on server timing headers being present ──────

    #[test]
    fn a4_row_present_with_ms_stats_when_server_timing_headers_exist() {
        let data = golden_report_data();
        let markdown = render_markdown(&data);
        assert!(markdown.contains("### A3, A4, and server handler overhead (measured, derived)"));
        assert!(markdown.contains("**A4 (measured):**"));
        assert!(markdown
            .contains("| quantity | n | mean (ms) | p50 (ms) | p95 (ms) | min (ms) | max (ms) |"));
        // Only trial 1 carries server_compute_ns (100_000 ns = 0.1 ms); trial
        // 2's header columns are blank and trial 3 is an errored row, excluded
        // from `successful` entirely.
        assert!(markdown.contains(
            "| A4 (server_compute_ns) | 1 | 0.1000 | 0.1000 | 0.1000 | 0.1000 | 0.1000 |"
        ));
    }

    #[test]
    fn a4_and_a3_absent_with_a_note_when_no_row_carries_server_timing_headers() {
        let trials_csv = trials_csv_with_rows(&[trial_csv_row(&[
            ("server_compute_ns", ""),
            ("server_handler_ns", ""),
        ])]);
        let client_blocks_csv = format!("{CLIENT_BLOCKS_HEADER}\n");
        let server_blocks_csv = format!("{SERVER_BLOCKS_HEADER}\n");
        let setup_json = r#"{
            "accounts": 1000,
            "buckets": 1024,
            "cells_bytes": 111111,
            "hint_bytes": 222222,
            "arity": 2,
            "bucket_size": 4,
            "lwe_dim": 999,
            "plaintext_bits": 8,
            "setup_seconds": 3.0,
            "persisted_hints_exact_match": true,
            "state_block": 300,
            "rayon_threads": 8
        }"#;
        let data = ReportData::parse(
            &trials_csv,
            &client_blocks_csv,
            &server_blocks_csv,
            setup_json,
            None,
            "",
            false,
        )
        .expect("valid fixture");
        let markdown = render_markdown(&data);
        assert!(markdown.contains(
            "No row in this dataset carries both `server_compute_ns` and `server_handler_ns`; \
             A4, A3, and server handler overhead are not computed."
        ));
        assert!(!markdown.contains("A4 (server_compute_ns)"));
        assert!(!markdown.contains("**A4 (measured):**"));
    }

    #[test]
    fn golden_markdown_states_the_percentile_method_and_n_semantics() {
        let data = golden_report_data();
        let markdown = render_markdown(&data);
        assert!(markdown.contains("nearest-rank"));
        assert!(markdown.contains("ceil(q*n)"));
        assert!(markdown.contains("error"), "must state what n excludes");
    }

    #[test]
    fn golden_markdown_reports_errored_and_coalesced_and_mismatch_counts() {
        let data = golden_report_data();
        let markdown = render_markdown(&data);
        // 1 errored trial (trial 3, batch 2, `DecodeFailed`) out of 3.
        assert!(markdown.contains("**Errored trials:** 1/3"));
        // 1 coalesced client-block row (block 105, blocks_in_fetch=5).
        assert!(markdown.contains("Coalesced client-block fetch rows excluded from B9/B10:** 1"));
        // 1 probe-adjacent server block (block 300, answers_since_prev_block=2).
        assert!(markdown.contains(
            "Blocks applied while probe traffic was active (`answers_since_prev_block > 0`):** 1/2"
        ));
        // 1 provider mismatch (trial 2, at_block 300).
        assert!(
            markdown.contains("`provider_match == 0` (independent-provider mismatch):** 1 row(s).")
        );
        assert!(markdown.contains("block 300, trial 2"));
        // Both successful trials balance exactly -> zero budget violation.
        assert!(markdown.contains("Max budget identity violation:** 0 us"));
        // The provenance fields are printed verbatim.
        assert!(markdown.contains("abc123"));
        assert!(markdown.contains("arity2 bucket4"));
    }

    #[test]
    fn golden_markdown_never_touches_input_strings() {
        // `render_markdown` takes `&ReportData` (parsed, owned data) and
        // returns a new `String` — there is no filesystem handle in this
        // call path at all, so "never modifies the inputs" is structural,
        // not just a convention. This test pins that shape: parsing twice
        // from the same raw strings yields the same markdown.
        let data1 = golden_report_data();
        let data2 = golden_report_data();
        assert_eq!(render_markdown(&data1), render_markdown(&data2));
    }

    // ── Provenance parsing ───────────────────────────────────────────────

    #[test]
    fn provenance_toml_like_parses_flat_key_values_in_order() {
        let raw = "# comment\ncommit = \"abc123\"\n[section]\nhost = value-without-quotes\n";
        let parsed = parse_provenance(raw, false).expect("valid fixture");
        assert_eq!(
            parsed,
            vec![
                ("commit".to_string(), "abc123".to_string()),
                ("host".to_string(), "value-without-quotes".to_string()),
            ]
        );
    }

    #[test]
    fn provenance_json_parses_flat_object() {
        let raw = r#"{"commit": "abc123", "block_min": 100}"#;
        let parsed = parse_provenance(raw, true).expect("valid fixture");
        assert!(parsed.contains(&("commit".to_string(), "abc123".to_string())));
        assert!(parsed.contains(&("block_min".to_string(), "100".to_string())));
    }
}
