//! Per-block CSV metrics (`--block-metrics-csv <path>`, off by default): a
//! measurement-campaign aid that appends exactly one row per successfully
//! applied mainnet block, so B7 (mutations/block, split insert/update/
//! delete), B8 (server apply time, split store/fold/patch), and B9 (delta
//! bytes published) can be read back and plotted from one run without
//! re-parsing the operational log.
//!
//! # Format
//!
//! Plain CSV, one header row (written only when the file is freshly
//! created or was empty — an existing non-empty file is appended to
//! as-is, so restarting a long-running follow loop against the same path
//! keeps accumulating one continuous file), then exactly one row per
//! applied block, each flushed to disk immediately after it is written
//! (`docs/plan.md`'s "never lose more than the current block" discipline,
//! applied to a measurement artifact rather than the state file). Columns,
//! in order — see [`BlockMetricsRow`]'s own field docs for exactly what
//! each one is:
//!
//! ```text
//! block,applied_at_unix_ms,changes,credits,inserts,updates,deletes,
//! noop_deletes,touched_cells,store_ms,fold_ms,patch_ms,apply_ms,
//! lock_wait_ms,delta_bytes,answers_since_prev_block,
//! answer_compute_ms_since_prev_block,feed_fetch_ms,finalized_block
//! ```
//!
//! Millisecond columns are floats with 3 decimals; every other column is
//! a plain unsigned integer. **No address and no balance ever appears in
//! a row** — [`BlockMetricsRow`] has no field shaped like one, so this is
//! true by construction, not by discipline (mirrors `crate::metrics`'
//! own privacy rule for `GET /metrics`, ADR-0039).

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;

/// The header row, written verbatim as [`BlockMetricsRow`]'s field order
/// — see this module's own docs for the full column list.
const HEADER: &str = "block,applied_at_unix_ms,changes,credits,inserts,updates,deletes,noop_deletes,touched_cells,store_ms,fold_ms,patch_ms,apply_ms,lock_wait_ms,delta_bytes,answers_since_prev_block,answer_compute_ms_since_prev_block,feed_fetch_ms,finalized_block";

/// Number of columns the header row and every [`BlockMetricsRow`] both
/// produce — checked directly by this module's own tests rather than
/// merely asserted in a doc comment.
pub const COLUMN_COUNT: usize = 19;

/// One applied block's worth of measurement columns — see this module's
/// docs for the exact on-disk column order, which mirrors this struct's
/// field order exactly.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct BlockMetricsRow {
    /// The applied block number.
    pub block: u64,
    /// Unix time, in milliseconds, when this row was written (immediately
    /// after the block finished applying).
    pub applied_at_unix_ms: u64,
    /// `BlockApplyReport::changes` — `update.changes.len()`.
    pub changes: u64,
    /// `BlockApplyReport::credits` — `update.credits.len()`.
    pub credits: u64,
    /// `BlockApplyReport::inserts`.
    pub inserts: u64,
    /// `BlockApplyReport::updates`.
    pub updates: u64,
    /// `BlockApplyReport::deletes`.
    pub deletes: u64,
    /// `BlockApplyReport::noop_deletes`.
    pub noop_deletes: u64,
    /// `BlockApplyReport::touched_cells` — B9's per-block numerator, the
    /// size of what a client rewinding through this block's delta will
    /// ingest.
    pub touched_cells: u64,
    /// `BlockApplyReport::store_dur`, milliseconds.
    pub store_ms: f64,
    /// `BlockApplyReport::fold_dur`, milliseconds.
    pub fold_ms: f64,
    /// `BlockApplyReport::patch_dur`, milliseconds.
    pub patch_ms: f64,
    /// B8: `NodeState::apply_block_reporting`'s own `apply_duration`
    /// (the whole `RisePirServer::apply_block_reporting` call), which by
    /// construction is `store_ms + fold_ms + patch_ms` plus a tiny
    /// residual — see that method's docs.
    pub apply_ms: f64,
    /// Time this block's `apply_block_reporting` call spent queued for
    /// `NodeState`'s write lock before `apply_ms` began.
    pub lock_wait_ms: f64,
    /// B9: `BlockDelta::encoded_len()` for this block — the exact byte
    /// length of the compact delta encoding served at `GET /delta/{block}`
    /// and folded into `/sync` and the on-disk journal.
    pub delta_bytes: u64,
    /// `/answer` computations served between the previous applied block
    /// and this one — an interference indicator (how much concurrent
    /// query traffic overlapped this block's apply).
    pub answers_since_prev_block: u64,
    /// Wall-clock milliseconds spent computing those `/answer` responses
    /// (the same clock `risepir_answer_duration_seconds` accumulates),
    /// over the same window as `answers_since_prev_block`.
    pub answer_compute_ms_since_prev_block: f64,
    /// Wall time of fetching this block's update from the feed
    /// (`RpcFeed::block_update`) — the follow loop has a natural point to
    /// time this (immediately around the successful fetch call), so this
    /// column is always populated for a live mainnet run; it exists as a
    /// plain `f64` rather than an `Option` because no caller in this
    /// crate constructs a row without a real feed-fetch measurement in
    /// hand.
    pub feed_fetch_ms: f64,
    /// The finalized head the follow loop was following at the moment
    /// this block was applied (i.e. `finalized` in `follow_loop`, not
    /// this row's own `block`).
    pub finalized_block: u64,
}

impl BlockMetricsRow {
    /// Renders this row as one CSV line, **without** a trailing newline —
    /// [`BlockMetricsCsvWriter::write_row`] appends that. Millisecond
    /// fields are formatted with exactly 3 decimals; every other field is
    /// a plain unsigned integer. Never contains an address or a balance —
    /// see this module's own docs.
    fn to_csv_line(self) -> String {
        format!(
            "{},{},{},{},{},{},{},{},{},{:.3},{:.3},{:.3},{:.3},{:.3},{},{},{:.3},{:.3},{}",
            self.block,
            self.applied_at_unix_ms,
            self.changes,
            self.credits,
            self.inserts,
            self.updates,
            self.deletes,
            self.noop_deletes,
            self.touched_cells,
            self.store_ms,
            self.fold_ms,
            self.patch_ms,
            self.apply_ms,
            self.lock_wait_ms,
            self.delta_bytes,
            self.answers_since_prev_block,
            self.answer_compute_ms_since_prev_block,
            self.feed_fetch_ms,
            self.finalized_block,
        )
    }
}

/// Appends [`BlockMetricsRow`]s to a plain CSV file, one per applied
/// block, each flushed immediately (`Self::write_row`). See the module
/// docs for the format and the header-once rule.
pub struct BlockMetricsCsvWriter {
    file: File,
}

impl BlockMetricsCsvWriter {
    /// Opens `path` for appending, creating it if it does not exist.
    /// Writes the header row first (and only) when the file is freshly
    /// created or was already empty — an existing non-empty file (e.g. a
    /// restart against the same `--block-metrics-csv` path) is appended
    /// to exactly as it stands, never re-headered and never truncated.
    pub fn open(path: &Path) -> io::Result<Self> {
        let mut file = OpenOptions::new().create(true).append(true).open(path)?;
        let needs_header = file.metadata()?.len() == 0;
        if needs_header {
            file.write_all(HEADER.as_bytes())?;
            file.write_all(b"\n")?;
            file.flush()?;
        }
        Ok(Self { file })
    }

    /// Appends one row and flushes immediately — a crash right after this
    /// call returns loses at most the *next* row, never this one.
    pub fn write_row(&mut self, row: BlockMetricsRow) -> io::Result<()> {
        self.file.write_all(row.to_csv_line().as_bytes())?;
        self.file.write_all(b"\n")?;
        self.file.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "risepir-block-metrics-csv-{}-{name}",
            std::process::id()
        ))
    }

    fn sample_row(block: u64) -> BlockMetricsRow {
        BlockMetricsRow {
            block,
            applied_at_unix_ms: 1_700_000_000_000,
            changes: 4,
            credits: 1,
            inserts: 1,
            updates: 2,
            deletes: 1,
            noop_deletes: 1,
            touched_cells: 37,
            store_ms: 0.512,
            fold_ms: 0.031,
            patch_ms: 1.204,
            apply_ms: 1.8,
            lock_wait_ms: 0.005,
            delta_bytes: 812,
            answers_since_prev_block: 3,
            answer_compute_ms_since_prev_block: 9.75,
            feed_fetch_ms: 42.125,
            finalized_block: block + 20,
        }
    }

    /// Opening on a fresh path, then appending a second time (simulating a
    /// restart against the same `--block-metrics-csv` file), must write
    /// [`HEADER`] exactly once — never per-open, never per-row.
    #[test]
    fn header_is_written_exactly_once_across_two_opens() {
        let path = tmp("header-once.csv");
        let _ = std::fs::remove_file(&path);

        {
            let mut w = BlockMetricsCsvWriter::open(&path).unwrap();
            w.write_row(sample_row(1)).unwrap();
        }
        {
            let mut w = BlockMetricsCsvWriter::open(&path).unwrap();
            w.write_row(sample_row(2)).unwrap();
        }

        let contents = std::fs::read_to_string(&path).unwrap();
        let header_lines = contents.lines().filter(|l| *l == HEADER).count();
        assert_eq!(
            header_lines, 1,
            "header must appear exactly once across two appending opens:\n{contents}"
        );
        // Both rows survived (append, never truncate).
        assert_eq!(contents.lines().count(), 3, "header + 2 rows");

        std::fs::remove_file(&path).ok();
    }

    /// Every row — header included — has exactly [`COLUMN_COUNT`] (19)
    /// comma-separated columns, matching the documented column list.
    #[test]
    fn every_row_has_exactly_nineteen_columns() {
        let path = tmp("column-count.csv");
        let _ = std::fs::remove_file(&path);

        {
            let mut w = BlockMetricsCsvWriter::open(&path).unwrap();
            w.write_row(sample_row(42)).unwrap();
        }

        let contents = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 2, "header + 1 row");
        for line in &lines {
            assert_eq!(
                line.split(',').count(),
                COLUMN_COUNT,
                "expected exactly {COLUMN_COUNT} columns in: {line}"
            );
        }

        std::fs::remove_file(&path).ok();
    }

    /// Milliseconds render as floats with exactly 3 decimals; plain counts
    /// render as bare integers — the documented format, checked directly
    /// rather than merely asserted.
    #[test]
    fn milliseconds_have_three_decimals_and_counts_are_plain_integers() {
        let row = sample_row(7);
        let line = row.to_csv_line();
        let cols: Vec<&str> = line.split(',').collect();
        assert_eq!(cols.len(), COLUMN_COUNT);
        // store_ms is column index 9 (0-based) per HEADER's own order.
        assert_eq!(cols[9], "0.512");
        assert_eq!(cols[12], "1.800", "apply_ms: 1.8 -> \"1.800\"");
        // block (0) and delta_bytes (14) are plain integers, no decimal point.
        assert_eq!(cols[0], "7");
        assert!(!cols[0].contains('.'));
        assert_eq!(cols[14], "812");
        assert!(!cols[14].contains('.'));
    }

    /// No 40+ hex-digit run (a 20-byte address, hex-encoded) can appear in
    /// a rendered row — the same tripwire shape `crate::metrics`'s own
    /// `nothing_rendered_looks_address_or_hex_blob_shaped` test uses for
    /// `GET /metrics` (ADR-0039), applied here to this CSV format. Trivial
    /// by construction (no field is address-shaped), but checked directly
    /// rather than only asserted in a doc comment — and pinned against a
    /// row carrying values that are themselves large enough to produce
    /// long digit runs, so the check is not vacuous.
    #[test]
    fn no_row_ever_contains_a_40_hex_digit_run() {
        // `u64::MAX - 20` rather than `u64::MAX` itself: `sample_row` sets
        // `finalized_block: block + 20`, which must not overflow.
        let mut row = sample_row(u64::MAX - 20);
        row.applied_at_unix_ms = u64::MAX;
        row.delta_bytes = u64::MAX;
        row.touched_cells = u64::MAX;
        row.finalized_block = u64::MAX;
        let line = row.to_csv_line();
        for token in line.split(|c: char| !c.is_ascii_hexdigit()) {
            assert!(
                token.len() < 40,
                "found a 40+ hex-digit run, address-shaped: {token:?} in: {line}"
            );
        }
    }
}
