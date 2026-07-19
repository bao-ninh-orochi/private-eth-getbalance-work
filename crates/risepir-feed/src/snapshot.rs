//! Snapshot ingest: stream `(address, eth_balance)` CSV rows — the shape
//! BigQuery's `crypto_ethereum.balances` export produces — into whatever
//! sink the caller provides (in the real deployment: straight into the
//! KV-SCF, ADR-0016; `docs/data-acquisition.md`, `docs/deploy.md`).
//!
//! # Input format
//!
//! One or more files, each plain CSV or gzip-compressed CSV (`.gz`
//! extension decides), rows of exactly two comma-separated fields:
//!
//! ```text
//! address,eth_balance
//! 0x4838b106fce9647bdf1e7877bf73ce8b0bad5f97,9843447628541445480
//! ```
//!
//! - An optional header row (`address,eth_balance`, case-insensitive) is
//!   skipped, per file — exactly what `bq extract --format CSV` emits.
//! - `address`: `0x`-prefixed, exactly 40 hex digits (either case).
//! - `eth_balance`: **decimal wei**. BigQuery's `NUMERIC` export may print
//!   a fractional part; it is accepted **only if it is all zeros**
//!   (`…wei.000000000`) — a genuinely fractional wei value is malformed
//!   input and hard-fails. Scientific notation hard-fails: this loader
//!   never guesses (`docs/plan.md`'s invariant applies to ingest too — a
//!   misparsed snapshot *is* a wrong answer, served later).
//!
//! # What the sink receives
//!
//! `(keccak256(address), balance)` for every row with `balance > 0`, in
//! file order. Zero-balance rows are counted and dropped (ADR-0015 — the
//! runbook's `WHERE eth_balance > 0` should already exclude them, but a
//! snapshot that includes them anyway must not turn into stored zeros).

use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};

use risepir_proto::{keccak256, AddressHash, Balance};

/// Errors from [`ingest`] / [`count_rows`]. Every variant names the file
/// (and, where meaningful, the 1-based line) so a multi-gigabyte ingest
/// failure is diagnosable without re-running under a debugger.
#[derive(Debug)]
pub enum SnapshotError {
    /// Opening or reading a file failed.
    Io {
        /// The file being read.
        file: PathBuf,
        /// The underlying I/O error, stringified.
        error: String,
    },
    /// A data row failed to parse. The snapshot is rejected outright —
    /// skipping rows would serve someone a wrong balance later.
    MalformedRow {
        /// The file containing the row.
        file: PathBuf,
        /// 1-based line number within that file.
        line: u64,
        /// What was wrong with it.
        reason: String,
    },
    /// The caller's sink rejected a row (e.g. the store hit `TableFull`
    /// because the geometry was sized for fewer accounts than the
    /// snapshot actually contains).
    Sink {
        /// The file whose row was being sunk.
        file: PathBuf,
        /// 1-based line number within that file.
        line: u64,
        /// The sink's own error message.
        error: String,
    },
}

impl std::fmt::Display for SnapshotError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io { file, error } => write!(f, "{}: {error}", file.display()),
            Self::MalformedRow { file, line, reason } => {
                write!(f, "{}:{line}: malformed row: {reason}", file.display())
            }
            Self::Sink { file, line, error } => {
                write!(f, "{}:{line}: sink rejected row: {error}", file.display())
            }
        }
    }
}

impl std::error::Error for SnapshotError {}

/// What [`ingest`] processed, for the operator log and for validating the
/// snapshot against the expectations that sized the geometry
/// (`docs/deploy.md`: the `bq` gate's `count(*)` should equal `nonzero`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SnapshotStats {
    /// Data rows parsed (headers excluded), across all files.
    pub rows: u64,
    /// Rows with `balance > 0`, i.e. rows handed to the sink.
    pub nonzero: u64,
    /// Rows with `balance == 0`, counted and dropped (ADR-0015).
    pub zero_skipped: u64,
    /// Largest balance seen — a cheap sanity signal against the codec's
    /// `balance_bits` (the sink's encode hard-fails on overflow anyway;
    /// this just makes the margin visible in the log).
    pub max_balance: Balance,
}

/// Streams every row of `paths` (in order) into `sink` as
/// `(keccak256(address), balance)`, skipping zero balances. See the
/// module docs for the accepted format.
///
/// # Errors
///
/// [`SnapshotError`] on the first I/O failure, malformed row, or sink
/// rejection — the ingest never skips a bad row and continues.
pub fn ingest<F>(paths: &[PathBuf], mut sink: F) -> Result<SnapshotStats, SnapshotError>
where
    F: FnMut(AddressHash, Balance) -> Result<(), String>,
{
    let mut stats = SnapshotStats::default();
    for path in paths {
        for_each_row(path, |line_no, addr20, balance| {
            stats.rows += 1;
            if balance == 0 {
                stats.zero_skipped += 1;
                return Ok(());
            }
            stats.nonzero += 1;
            stats.max_balance = stats.max_balance.max(balance);
            sink(keccak256(&addr20), balance).map_err(|error| SnapshotError::Sink {
                file: path.clone(),
                line: line_no,
                error,
            })
        })?;
    }
    Ok(stats)
}

/// Counts data rows (headers excluded) across `paths` without hashing or
/// sinking anything — the cheap first pass that lets a deployment size
/// its geometry from the snapshot itself instead of trusting a
/// separately-supplied count. Zero-balance rows are counted too (they
/// parse like any other row); geometry sized from this count is therefore
/// never too small.
pub fn count_rows(paths: &[PathBuf]) -> Result<u64, SnapshotError> {
    let mut rows = 0u64;
    for path in paths {
        for_each_row(path, |_, _, _| {
            rows += 1;
            Ok(())
        })?;
    }
    Ok(rows)
}

/// Opens `path` (gzip-decoding iff the extension is `.gz`) and drives
/// `f(line_no, address, balance)` over every data row.
fn for_each_row<F>(path: &Path, mut f: F) -> Result<(), SnapshotError>
where
    F: FnMut(u64, [u8; 20], Balance) -> Result<(), SnapshotError>,
{
    let file = File::open(path).map_err(|e| SnapshotError::Io {
        file: path.to_path_buf(),
        error: e.to_string(),
    })?;
    let reader: Box<dyn Read> = if path.extension().is_some_and(|e| e == "gz") {
        Box::new(flate2::read::GzDecoder::new(file))
    } else {
        Box::new(file)
    };
    let reader = BufReader::new(reader);

    let mut line_no = 0u64;
    for line in reader.lines() {
        line_no += 1;
        let line = line.map_err(|e| SnapshotError::Io {
            file: path.to_path_buf(),
            error: format!("line {line_no}: {e}"),
        })?;
        let row = line.strip_suffix('\r').unwrap_or(&line);
        if row.is_empty() {
            continue; // trailing newline at EOF etc.
        }
        if line_no == 1 && row.eq_ignore_ascii_case("address,eth_balance") {
            continue; // per-file header
        }
        let (addr20, balance) = parse_row(row).map_err(|reason| SnapshotError::MalformedRow {
            file: path.to_path_buf(),
            line: line_no,
            reason,
        })?;
        f(line_no, addr20, balance)?;
    }
    Ok(())
}

/// Parses one `address,eth_balance` row. See the module docs for the
/// accepted shapes; anything else is an error, never a guess.
fn parse_row(row: &str) -> Result<([u8; 20], Balance), String> {
    let mut fields = row.split(',');
    let (Some(addr_str), Some(bal_str), None) = (fields.next(), fields.next(), fields.next()) else {
        return Err("expected exactly 2 comma-separated fields".to_string());
    };
    let addr20 = parse_address(addr_str.trim())
        .ok_or_else(|| format!("address must be 0x + 40 hex digits, got {addr_str:?}"))?;
    let balance = parse_wei(bal_str.trim())?;
    Ok((addr20, balance))
}

/// `0x`-prefixed, exactly-40-hex-digit Ethereum address (either case).
fn parse_address(s: &str) -> Option<[u8; 20]> {
    let hex = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X"))?;
    if hex.len() != 40 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut out = [0u8; 20];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

/// Decimal wei, with BigQuery-`NUMERIC`'s all-zero fractional part
/// tolerated (`123.000000000` ⇒ `123`); everything else — scientific
/// notation, a nonzero fraction, signs, hex — hard-fails.
fn parse_wei(s: &str) -> Result<Balance, String> {
    let (int_part, frac_part) = match s.split_once('.') {
        Some((i, f)) => (i, Some(f)),
        None => (s, None),
    };
    if int_part.is_empty() || !int_part.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!("balance must be decimal wei, got {s:?}"));
    }
    if let Some(frac) = frac_part {
        if frac.is_empty() || !frac.bytes().all(|b| b == b'0') {
            return Err(format!(
                "balance has a nonzero (or malformed) fractional part, got {s:?} — refusing to round wei"
            ));
        }
    }
    int_part
        .parse::<Balance>()
        .map_err(|_| format!("balance does not fit u128, got {s:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Unique temp file with `contents`; caller removes it. `.gz` in
    /// `name` gzip-compresses the contents, mirroring how the loader
    /// decides to decompress.
    fn temp_file(name: &str, contents: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "risepir-snapshot-{}-{name}",
            std::process::id()
        ));
        if name.ends_with(".gz") {
            let f = File::create(&path).unwrap();
            let mut enc = flate2::write::GzEncoder::new(f, flate2::Compression::fast());
            enc.write_all(contents.as_bytes()).unwrap();
            enc.finish().unwrap();
        } else {
            std::fs::write(&path, contents).unwrap();
        }
        path
    }

    fn collect(paths: &[PathBuf]) -> Result<(Vec<(AddressHash, Balance)>, SnapshotStats), SnapshotError> {
        let mut rows = Vec::new();
        let stats = ingest(paths, |k, v| {
            rows.push((k, v));
            Ok(())
        })?;
        Ok((rows, stats))
    }

    const A1: &str = "0x4838b106fce9647bdf1e7877bf73ce8b0bad5f97";
    const A2: &str = "0x80113991FFB2AE61F5B42199D80AF0B9BC3B5A87"; // upper-hex on purpose

    fn addr20(s: &str) -> [u8; 20] {
        parse_address(s).unwrap()
    }

    #[test]
    fn parses_header_plain_and_gz_identically() {
        let body = format!("address,eth_balance\n{A1},100\n{A2},9843447628541445480\n");
        let plain = temp_file("a.csv", &body);
        let gz = temp_file("a.csv.gz", &body);

        let (rows_p, stats_p) = collect(std::slice::from_ref(&plain)).unwrap();
        let (rows_g, stats_g) = collect(std::slice::from_ref(&gz)).unwrap();
        assert_eq!(rows_p, rows_g, "gzip and plain must parse identically");
        assert_eq!(stats_p, stats_g);
        assert_eq!(stats_p.rows, 2);
        assert_eq!(stats_p.nonzero, 2);
        assert_eq!(rows_p[0], (keccak256(&addr20(A1)), 100u128));
        assert_eq!(rows_p[1].1, 9_843_447_628_541_445_480u128);

        std::fs::remove_file(plain).unwrap();
        std::fs::remove_file(gz).unwrap();
    }

    #[test]
    fn zero_rows_are_skipped_not_sunk() {
        let f = temp_file("zero.csv", &format!("{A1},0\n{A2},5\n"));
        let (rows, stats) = collect(std::slice::from_ref(&f)).unwrap();
        assert_eq!(stats.rows, 2);
        assert_eq!(stats.zero_skipped, 1);
        assert_eq!(stats.nonzero, 1);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, keccak256(&addr20(A2)));
        std::fs::remove_file(f).unwrap();
    }

    #[test]
    fn numeric_export_all_zero_fraction_accepted_nonzero_rejected() {
        let ok = temp_file("frac-ok.csv", &format!("{A1},123.000000000\n"));
        let (rows, _) = collect(std::slice::from_ref(&ok)).unwrap();
        assert_eq!(rows[0].1, 123);
        std::fs::remove_file(ok).unwrap();

        let bad = temp_file("frac-bad.csv", &format!("{A1},123.5\n"));
        let err = collect(std::slice::from_ref(&bad)).unwrap_err();
        assert!(matches!(err, SnapshotError::MalformedRow { line: 1, .. }), "{err}");
        std::fs::remove_file(bad).unwrap();
    }

    #[test]
    fn malformed_rows_hard_fail_with_position() {
        for (name, body, want_line) in [
            ("sci.csv", format!("{A1},1.0E+21\n"), 1),
            ("hexbal.csv", format!("{A1},0x10\n"), 1),
            ("neg.csv", format!("{A1},-5\n"), 1),
            ("shortaddr.csv", "0x1234,10\n".to_string(), 1),
            ("noprefix.csv", "4838b106fce9647bdf1e7877bf73ce8b0bad5f97,10\n".to_string(), 1),
            ("threefields.csv", format!("{A1},10,extra\n"), 1),
            ("late.csv", format!("{A1},10\ngarbage\n"), 2),
        ] {
            let f = temp_file(name, &body);
            let err = collect(std::slice::from_ref(&f)).unwrap_err();
            match err {
                SnapshotError::MalformedRow { line, .. } => {
                    assert_eq!(line, want_line, "{name}: wrong line reported")
                }
                other => panic!("{name}: expected MalformedRow, got {other}"),
            }
            std::fs::remove_file(f).unwrap();
        }
    }

    #[test]
    fn header_only_skipped_on_first_line() {
        // A row that *looks* like a header but is not on line 1 is data —
        // and fails to parse, loudly.
        let f = temp_file("midheader.csv", &format!("{A1},10\naddress,eth_balance\n"));
        let err = collect(std::slice::from_ref(&f)).unwrap_err();
        assert!(matches!(err, SnapshotError::MalformedRow { line: 2, .. }));
        std::fs::remove_file(f).unwrap();
    }

    #[test]
    fn multiple_shards_in_order_and_count_rows_matches() {
        let f1 = temp_file("s1.csv", &format!("address,eth_balance\n{A1},1\n"));
        let f2 = temp_file("s2.csv", &format!("address,eth_balance\n{A2},2\n{A1},0\n"));
        let paths = vec![f1.clone(), f2.clone()];

        let (rows, stats) = collect(&paths).unwrap();
        assert_eq!(stats.rows, 3);
        assert_eq!(rows.iter().map(|r| r.1).collect::<Vec<_>>(), vec![1, 2]);
        assert_eq!(count_rows(&paths).unwrap(), 3, "count_rows counts zero rows too");

        std::fs::remove_file(f1).unwrap();
        std::fs::remove_file(f2).unwrap();
    }

    #[test]
    fn sink_error_carries_file_and_line() {
        let f = temp_file("sink.csv", &format!("{A1},1\n{A2},2\n"));
        let err = ingest(std::slice::from_ref(&f), |_, v| {
            if v == 2 {
                Err("table full".to_string())
            } else {
                Ok(())
            }
        })
        .unwrap_err();
        match err {
            SnapshotError::Sink { line, error, .. } => {
                assert_eq!(line, 2);
                assert_eq!(error, "table full");
            }
            other => panic!("expected Sink, got {other}"),
        }
        std::fs::remove_file(f).unwrap();
    }

    #[test]
    fn u128_overflow_balance_rejected() {
        let f = temp_file("big.csv", &format!("{A1},340282366920938463463374607431768211456\n")); // 2^128
        let err = collect(std::slice::from_ref(&f)).unwrap_err();
        assert!(matches!(err, SnapshotError::MalformedRow { .. }));
        std::fs::remove_file(f).unwrap();
    }
}
