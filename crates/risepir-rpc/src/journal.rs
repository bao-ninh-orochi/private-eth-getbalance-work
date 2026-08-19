//! Sidecar append-only delta journal (ADR-0026): `<state>.journal` records
//! one [`BlockDelta`] per successfully applied block, bound to a specific
//! base state file by its trailing `RPST2` digest
//! ([`crate::state::SaveReport::digest`]).
//!
//! # Why this exists
//!
//! `H` (the per-segment PIR hint) is a deterministic function of the
//! delta stream (`server_patch_hint`'s documented contract), so a journal
//! only has to store the ~15-30 KB/block *semantic* delta, never the
//! ~25-40 MB/block of literal `H` + cell-array byte churn a periodic full
//! save costs. With replay trusted, an operator can raise
//! `--save-interval` from minutes to hours and still recover to within
//! seconds of the last applied block after a crash — full rationale in
//! `docs/adr/README.md` ADR-0026.
//!
//! # Format `RPJL1` (all integers little-endian)
//!
//! ```text
//! header:  magic b"RPJL1"
//!          u64  base_digest      // the RPST2 trailing xxh3 of the base file this extends
//!          u64  base_block       // base height; first record must be base_block+1
//!          u64  xxh3 of the 21 bytes above
//! records: u32  len              // payload byte length; 0 < len <= MAX_RECORD_BYTES,
//!                                 // also bounded by remaining file size before allocating
//!          u64  num_items_after  // server.num_items() after applying this block
//!          [len bytes]           // risepir_proto::codec::encode_block_delta(&delta, plaintext_bits)
//!          u64  xxh3 over (num_items_after_le ‖ payload)
//! ```
//!
//! Record height is the decoded `BlockDelta.block`; continuity is
//! enforced (each record must be exactly one more than the previous,
//! starting at `base_block + 1`) both at write time ([`JournalWriter::append`])
//! and at read time ([`JournalReader`]).
//!
//! # The two failure classes (never a silently wrong answer)
//!
//! - **Pre-apply validation failure** — a bad header, a base mismatch, a
//!   bad record checksum, a decode error, a length violation, a height
//!   gap, or a torn/truncated tail: [`JournalReader`] *stops* (never
//!   errors the whole read) at the first such record and reports exactly
//!   where and why ([`ScanStop::Invalid`]). Every record before that
//!   point is valid and usable; the caller falls back to network replay
//!   for the rest — staleness, never a wrong answer.
//! - **Apply-time failure** — a record that passed every pre-apply check
//!   but whose cell delta would take a cell outside `[0, 2^plaintext_bits)`
//!   (`crate::state::RestoreError::ApplyFailure`, checked by the replay
//!   loop in `crate::state::load_with_journal_restore`, not by this
//!   module): the in-memory state being mutated is now torn mid-block,
//!   which is serious enough to refuse to serve at all rather than fall
//!   back — see that function's docs.

use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use risepir_proto::BlockDelta;

const MAGIC: &[u8; 5] = b"RPJL1";
/// Header length in bytes: `magic(5) + base_digest(8) + base_block(8) + xxh3(8)`.
const HEADER_LEN: u64 = 5 + 8 + 8 + 8;
/// Per-record payload cap, checked *before* any allocation is sized from
/// the on-disk `len` field — the repo's validate-every-length rule
/// applies to the journal exactly as it does to the state file and every
/// wire codec. 64 MiB is generous next to the ~15-30 KB/block this format
/// is designed around.
pub const MAX_RECORD_BYTES: u64 = 64 * 1024 * 1024;

/// Errors from [`JournalWriter`] / [`JournalReader::open`].
#[derive(Debug)]
pub enum JournalError {
    /// File open/read/write/rename/fsync failure.
    Io(String),
    /// The file is not a well-formed `RPJL1` journal: too short to hold a
    /// header at all, wrong magic, or a header whose own checksum does
    /// not match.
    Corrupt(String),
    /// [`JournalWriter::append`] was asked to write a block that does not
    /// extend the journal by exactly one. Nothing is written when this is
    /// returned; the caller must not call `append` again this run without
    /// first rotating to a fresh journal — a gapped file would be a
    /// wrong-answer trap at the next restore, never merely stale data.
    Gap {
        /// The height continuity required (`last() + 1`).
        expected: u64,
        /// The height actually offered.
        found: u64,
    },
}

impl std::fmt::Display for JournalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(m) => write!(f, "journal I/O: {m}"),
            Self::Corrupt(m) => write!(f, "journal rejected: {m}"),
            Self::Gap { expected, found } => {
                write!(
                    f,
                    "journal continuity violated: expected block {expected}, got {found}"
                )
            }
        }
    }
}

impl std::error::Error for JournalError {}

fn io_err(e: impl std::fmt::Display) -> JournalError {
    JournalError::Io(e.to_string())
}

/// Derives a state file's journal sidecar path: `foo.bin` -> `foo.journal`
/// (`docs/adr/README.md` ADR-0026). Mirrors the existing `<path>.tmp`
/// convention, also via [`Path::with_extension`].
pub fn journal_path_for(state_path: &Path) -> PathBuf {
    state_path.with_extension("journal")
}

/// `<path>` with a literal `.tmp` appended (not [`Path::with_extension`]):
/// for a journal path `foo.journal`, `with_extension("tmp")` would give
/// `foo.tmp` — byte-for-byte the *same* staging path the state file's own
/// save uses for `foo.bin`. The two writes never overlap in practice
/// (journal rotation runs strictly after the base save's rename
/// completes — see `autosave.rs`'s module docs), but giving the journal
/// its own distinct staging name removes the coincidence entirely rather
/// than leaning on that ordering to keep two unrelated writers apart.
fn tmp_sibling(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".tmp");
    PathBuf::from(s)
}

/// The two fields a [`JournalWriter::create`]d file's header commits to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JournalHeader {
    /// The base state file's trailing `RPST2` xxh3 digest
    /// (`crate::state::SaveReport::digest`) at the moment this journal
    /// was created — the binding that lets a loader refuse a journal
    /// that extends some *other* save (never a silently wrong answer).
    pub base_digest: u64,
    /// The base state file's block height at that same moment. The first
    /// record, if any, must carry `base_block + 1`.
    pub base_block: u64,
}

/// Why a [`JournalReader`] stopped producing records — see
/// [`JournalReader::stop`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScanStop {
    /// Clean end of file; every record from the header on was valid.
    Eof,
    /// The record beginning at byte `offset` (measured from the start of
    /// the file, header included) failed a pre-apply validation check
    /// (`reason` is a human-readable diagnosis for the loud startup log
    /// line). Everything before `offset` is valid and safe to use;
    /// [`JournalWriter::adopt`] truncates to exactly this offset.
    Invalid {
        /// Byte offset of the first invalid record.
        offset: u64,
        /// Human-readable diagnosis.
        reason: String,
    },
}

/// One validated record: the decoded delta, the item count the source
/// server reported immediately after applying it, and the byte offset
/// immediately following this record.
#[derive(Debug, Clone)]
pub struct ScanRecord {
    /// The decoded per-block delta.
    pub delta: BlockDelta,
    /// `server.num_items()` immediately after this block was applied.
    /// Replay cannot derive this from cell deltas alone (a delete and an
    /// insert both look like sparse cell writes to the fold step), so it
    /// rides along on the wire — see the module docs' format.
    pub num_items_after: u64,
    /// Byte offset, from the start of the file, immediately after this
    /// record — what [`JournalWriter::adopt`] would truncate to if this
    /// is the last good record.
    pub end_offset: u64,
}

/// Streaming, validating reader over an `RPJL1` journal: never
/// materializes the whole file (one record is decoded at a time and
/// dropped once the caller has consumed it), stops — rather than erroring
/// the whole read — at the first invalid record, and reports exactly
/// where and why.
///
/// # Usage
///
/// [`Self::open`] validates the header and returns `(header, reader)`;
/// drive `reader` as a plain [`Iterator`]. Once iteration ends
/// (`next()` returns `None`), [`Self::stop`] reports whether that was a
/// clean [`ScanStop::Eof`] or a validation failure, and
/// [`Self::valid_end_offset`] / [`Self::last_valid_height`] give the
/// usable prefix's boundary (what an `adopt` call needs).
#[derive(Debug)]
pub struct JournalReader<R> {
    reader: R,
    plaintext_bits: u32,
    arity: u32,
    remaining: u64,
    consumed: u64,
    expected_block: u64,
    done: bool,
    stop: Option<ScanStop>,
}

impl<R: Read> JournalReader<R> {
    /// Validates the header and returns it alongside a fresh reader
    /// positioned right after it. `total_len` must be `reader`'s exact
    /// total byte length — used to bound every subsequent length field
    /// before it sizes an allocation, the same discipline `state.rs`
    /// applies to the state file.
    ///
    /// `plaintext_bits` / `arity` come from the *base*'s own setup params
    /// (the caller's job — the journal format does not repeat them, since
    /// they never change without a fresh base) and are needed to decode
    /// each record's [`risepir_proto::codec::encode_block_delta`] payload.
    ///
    /// # Errors
    ///
    /// [`JournalError::Io`] on a read failure. [`JournalError::Corrupt`]
    /// if the file is shorter than a header, has the wrong magic, or
    /// fails its own header checksum.
    ///
    /// Matching the *base* this journal is bound to
    /// (`base_digest`/`base_block`) is deliberately **not** checked
    /// here — this reader has no notion of "the current base"; the
    /// caller compares the returned [`JournalHeader`] against its own
    /// loaded state before trusting anything else this reader produces.
    pub fn open(
        mut reader: R,
        total_len: u64,
        plaintext_bits: u32,
        arity: u32,
    ) -> Result<(JournalHeader, Self), JournalError> {
        if total_len < HEADER_LEN {
            return Err(JournalError::Corrupt(format!(
                "file too short for a journal header ({total_len} bytes, need {HEADER_LEN})"
            )));
        }
        let mut hasher = xxhash_rust::xxh3::Xxh3::new();
        let mut magic = [0u8; 5];
        reader.read_exact(&mut magic).map_err(io_err)?;
        if &magic != MAGIC {
            return Err(JournalError::Corrupt(
                "bad magic (not an RPJL1 journal)".to_string(),
            ));
        }
        hasher.update(&magic);
        let mut bd_bytes = [0u8; 8];
        reader.read_exact(&mut bd_bytes).map_err(io_err)?;
        hasher.update(&bd_bytes);
        let base_digest = u64::from_le_bytes(bd_bytes);
        let mut bb_bytes = [0u8; 8];
        reader.read_exact(&mut bb_bytes).map_err(io_err)?;
        hasher.update(&bb_bytes);
        let base_block = u64::from_le_bytes(bb_bytes);
        let mut stored_bytes = [0u8; 8];
        reader.read_exact(&mut stored_bytes).map_err(io_err)?;
        let stored = u64::from_le_bytes(stored_bytes);
        if hasher.digest() != stored {
            return Err(JournalError::Corrupt(
                "header checksum mismatch".to_string(),
            ));
        }

        let header = JournalHeader {
            base_digest,
            base_block,
        };
        let this = Self {
            reader,
            plaintext_bits,
            arity,
            remaining: total_len - HEADER_LEN,
            consumed: HEADER_LEN,
            expected_block: base_block + 1,
            done: false,
            stop: None,
        };
        Ok((header, this))
    }

    /// Byte offset, from the start of the file, immediately after the
    /// last successfully validated record (or the header length if none
    /// validated) — where [`JournalWriter::adopt`] truncates to.
    pub fn valid_end_offset(&self) -> u64 {
        self.consumed
    }

    /// Block height of the last successfully validated record, or the
    /// base height if none validated — the continuity seed
    /// [`JournalWriter::adopt`] resumes appending from.
    pub fn last_valid_height(&self) -> u64 {
        self.expected_block - 1
    }

    /// Why iteration stopped. `None` before iteration has run to
    /// completion (i.e. before `next()` has returned `None` at least
    /// once).
    pub fn stop(&self) -> Option<&ScanStop> {
        self.stop.as_ref()
    }

    /// Parses one more record. `Ok(None)` is a clean EOF at a record
    /// boundary — never an error: an empty journal (header only) is
    /// valid. `Err((reason, offset))` reports a pre-apply validation
    /// failure at byte `offset`.
    fn try_next(&mut self) -> Result<Option<ScanRecord>, (String, u64)> {
        if self.remaining == 0 {
            return Ok(None);
        }
        if self.remaining < 4 {
            return Err((
                "torn tail: fewer than 4 bytes left for a record length prefix".to_string(),
                self.consumed,
            ));
        }
        let mut len_bytes = [0u8; 4];
        self.reader.read_exact(&mut len_bytes).map_err(|e| {
            (
                format!("I/O error reading record length: {e}"),
                self.consumed,
            )
        })?;
        self.remaining -= 4;
        let len = u32::from_le_bytes(len_bytes);
        if len == 0 || u64::from(len) > MAX_RECORD_BYTES {
            return Err((
                format!("record length {len} out of bounds (0 < len <= {MAX_RECORD_BYTES})"),
                self.consumed,
            ));
        }
        // Remaining-file-size bound, checked *before* the payload
        // allocation below: num_items_after(8) + payload(len) + the
        // trailing xxh3(8) must all still fit in what is left.
        let need = 8u64 + u64::from(len) + 8u64;
        if need > self.remaining {
            return Err((
                format!(
                    "declared record length {len} exceeds the remaining file size ({} bytes left)",
                    self.remaining
                ),
                self.consumed,
            ));
        }

        let mut n_bytes = [0u8; 8];
        self.reader.read_exact(&mut n_bytes).map_err(|e| {
            (
                format!("I/O error reading num_items_after: {e}"),
                self.consumed,
            )
        })?;
        self.remaining -= 8;
        let num_items_after = u64::from_le_bytes(n_bytes);

        // Bounded above by both MAX_RECORD_BYTES and the input's own
        // remaining size (both checked above) before this allocation.
        let mut payload = vec![0u8; len as usize];
        self.reader.read_exact(&mut payload).map_err(|e| {
            (
                format!("I/O error reading record payload: {e}"),
                self.consumed,
            )
        })?;
        self.remaining -= u64::from(len);

        let mut xxh_bytes = [0u8; 8];
        self.reader.read_exact(&mut xxh_bytes).map_err(|e| {
            (
                format!("I/O error reading record checksum: {e}"),
                self.consumed,
            )
        })?;
        self.remaining -= 8;
        let stored = u64::from_le_bytes(xxh_bytes);

        let mut hasher = xxhash_rust::xxh3::Xxh3::new();
        hasher.update(&n_bytes);
        hasher.update(&payload);
        if hasher.digest() != stored {
            return Err((
                "record checksum mismatch (corrupt or torn record)".to_string(),
                self.consumed,
            ));
        }

        let delta =
            risepir_proto::codec::decode_block_delta(&payload, self.plaintext_bits, self.arity)
                .map_err(|e| (format!("record payload decode failed: {e}"), self.consumed))?;

        if delta.block != self.expected_block {
            return Err((
                format!(
                    "height gap: expected block {}, record carries block {}",
                    self.expected_block, delta.block
                ),
                self.consumed,
            ));
        }

        self.consumed += 4 + 8 + u64::from(len) + 8;
        self.expected_block += 1;
        Ok(Some(ScanRecord {
            delta,
            num_items_after,
            end_offset: self.consumed,
        }))
    }
}

impl<R: Read> Iterator for JournalReader<R> {
    type Item = ScanRecord;

    fn next(&mut self) -> Option<ScanRecord> {
        if self.done {
            return None;
        }
        match self.try_next() {
            Ok(Some(rec)) => Some(rec),
            Ok(None) => {
                self.done = true;
                self.stop = Some(ScanStop::Eof);
                None
            }
            Err((reason, offset)) => {
                self.done = true;
                self.stop = Some(ScanStop::Invalid { offset, reason });
                None
            }
        }
    }
}

/// Summary [`scan_report_only`] returns when a journal's header matches
/// the caller's base.
#[derive(Debug, Clone)]
pub struct JournalReportOnly {
    /// Number of valid records found beyond the base.
    pub count: u64,
    /// Height of the last valid record, or the base height if `count == 0`.
    pub end_height: u64,
    /// Byte offset immediately after the last valid record — what
    /// [`JournalWriter::adopt`] would truncate to.
    pub end_offset: u64,
    /// Why the scan stopped (always populated: the scan always runs to
    /// completion).
    pub stop: ScanStop,
}

/// Convenience wrapper for the `--journal-restore`-off report path
/// (`mainnet.rs`): opens `path`, and if its header matches
/// `base_digest`, consumes the whole scan (streaming — never more than
/// one record materialized at a time) and returns a summary.
///
/// `Ok(None)` covers every "nothing to report" case uniformly: the file
/// does not exist, its header is corrupt, or its header does not match
/// this base — all handled identically by the caller (ignore the file,
/// proceed exactly as if there were no journal, no alarming log).
///
/// # Errors
///
/// Only a genuine I/O failure other than "file does not exist" — an
/// unreadable-but-present file (permissions, a disk error) is distinct
/// from "no journal was ever created here".
pub fn scan_report_only(
    path: &Path,
    base_digest: u64,
    plaintext_bits: u32,
    arity: u32,
) -> std::io::Result<Option<JournalReportOnly>> {
    let file = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let total_len = file.metadata()?.len();
    let (header, mut reader) = match JournalReader::open(
        std::io::BufReader::new(file),
        total_len,
        plaintext_bits,
        arity,
    ) {
        Ok(pair) => pair,
        Err(_) => return Ok(None),
    };
    if header.base_digest != base_digest {
        return Ok(None);
    }
    let mut count = 0u64;
    while reader.next().is_some() {
        count += 1;
    }
    let stop = reader.stop().cloned().expect(
        "iterator exhausted above; stop() is always populated once next() has returned None",
    );
    Ok(Some(JournalReportOnly {
        count,
        end_height: reader.last_valid_height(),
        end_offset: reader.valid_end_offset(),
        stop,
    }))
}

/// Append-only writer for one journal file's lifetime — one instance per
/// rotation (a fresh [`Self::create`]) or per adopted restart
/// ([`Self::adopt`]); [`crate::autosave::StateSaver`] owns the current one
/// and replaces it wholesale at the next rotation.
pub struct JournalWriter {
    file: File,
    path: PathBuf,
    plaintext_bits: u32,
    last: u64,
}

impl JournalWriter {
    /// Starts a brand-new journal at `path`, header-committed to
    /// `(base_digest, base_block)`: writes the header to a `.tmp`
    /// sibling, fsyncs, renames into place (same discipline as
    /// `state::save`), then reopens `path` in append mode. Used at every
    /// rotation (a fresh journal begins exactly where a full save just
    /// landed) and right after a snapshot bootstrap's first save.
    ///
    /// # Errors
    ///
    /// [`JournalError::Io`] on any write/fsync/rename/reopen failure.
    pub fn create(
        path: &Path,
        base_digest: u64,
        base_block: u64,
        plaintext_bits: u32,
    ) -> Result<Self, JournalError> {
        let tmp = tmp_sibling(path);
        {
            let file = File::create(&tmp).map_err(io_err)?;
            let mut w = BufWriter::new(file);
            let mut hasher = xxhash_rust::xxh3::Xxh3::new();
            w.write_all(MAGIC).map_err(io_err)?;
            hasher.update(MAGIC);
            w.write_all(&base_digest.to_le_bytes()).map_err(io_err)?;
            hasher.update(&base_digest.to_le_bytes());
            w.write_all(&base_block.to_le_bytes()).map_err(io_err)?;
            hasher.update(&base_block.to_le_bytes());
            w.write_all(&hasher.digest().to_le_bytes())
                .map_err(io_err)?;
            w.flush().map_err(io_err)?;
            w.get_ref().sync_all().map_err(io_err)?;
        }
        std::fs::rename(&tmp, path).map_err(io_err)?;
        crate::state::fsync_parent_dir(path);
        let file = OpenOptions::new().append(true).open(path).map_err(io_err)?;
        Ok(Self {
            file,
            path: path.to_path_buf(),
            plaintext_bits,
            last: base_block,
        })
    }

    /// Resumes an existing on-disk journal whose header the caller has
    /// already matched against its currently loaded base: truncates away
    /// any bad tail (`set_len(valid_end_offset)` — a no-op if the scan
    /// reached a clean EOF) and continues appending after
    /// `last_valid_height`. The caller (`mainnet.rs` /
    /// `crate::state::load_with_journal_restore`) is the one that decided
    /// this file is safe to resume.
    ///
    /// # Errors
    ///
    /// [`JournalError::Io`] on any open/truncate failure.
    pub fn adopt(
        path: &Path,
        plaintext_bits: u32,
        valid_end_offset: u64,
        last_valid_height: u64,
    ) -> Result<Self, JournalError> {
        let file = OpenOptions::new().append(true).open(path).map_err(io_err)?;
        file.set_len(valid_end_offset).map_err(io_err)?;
        Ok(Self {
            file,
            path: path.to_path_buf(),
            plaintext_bits,
            last: last_valid_height,
        })
    }

    /// The path this writer is appending to (for log messages).
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The last height this writer has recorded (the base height if
    /// nothing has been appended yet).
    pub fn last(&self) -> u64 {
        self.last
    }

    /// Encodes and appends one record, `fsync`ing before returning so a
    /// crash right after can never see a record whose bytes never
    /// reached disk (same rationale as `state::save`'s pre-rename fsync).
    ///
    /// # Errors
    ///
    /// [`JournalError::Gap`] if `delta.block != self.last() + 1` —
    /// **nothing is written** in that case; the caller must not call this
    /// again this run without first rotating to a fresh journal (module
    /// docs: a gapped file is a wrong-answer trap at the next restore,
    /// never merely stale). [`JournalError::Io`] on any
    /// write/flush/sync failure — the record may or may not be fully on
    /// disk; the same "stop trusting this run's journal" response
    /// applies.
    pub fn append(&mut self, delta: &BlockDelta, num_items_after: u64) -> Result<(), JournalError> {
        let expected = self.last + 1;
        if delta.block != expected {
            return Err(JournalError::Gap {
                expected,
                found: delta.block,
            });
        }

        let payload = risepir_proto::codec::encode_block_delta(delta, self.plaintext_bits);
        let len = u32::try_from(payload.len()).map_err(|_| {
            JournalError::Io(format!(
                "record payload {} bytes exceeds u32::MAX",
                payload.len()
            ))
        })?;

        let mut hasher = xxhash_rust::xxh3::Xxh3::new();
        let n_bytes = num_items_after.to_le_bytes();
        hasher.update(&n_bytes);
        hasher.update(&payload);
        let xxh3 = hasher.digest();

        self.file.write_all(&len.to_le_bytes()).map_err(io_err)?;
        self.file.write_all(&n_bytes).map_err(io_err)?;
        self.file.write_all(&payload).map_err(io_err)?;
        self.file.write_all(&xxh3.to_le_bytes()).map_err(io_err)?;
        self.file.flush().map_err(io_err)?;
        self.file.sync_data().map_err(io_err)?;

        self.last = delta.block;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    const PB: u32 = 8;
    const ARITY: u32 = 2;

    fn delta(block: u64, magnitude: i64) -> BlockDelta {
        BlockDelta {
            block,
            // ARITY (2) segments — `JournalReader::next` decodes each
            // record's payload against `self.arity` (`ARITY` here), so a
            // segment count that disagrees is a wire-level ArityMismatch,
            // not merely an unrealistic fixture.
            per_segment: vec![vec![(0, vec![(0, magnitude)])], vec![]],
        }
    }

    fn tmp(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("risepir-journal-{}-{name}", std::process::id()))
    }

    fn open_scan(path: &Path) -> (JournalHeader, JournalReader<File>) {
        let file = File::open(path).unwrap();
        let len = file.metadata().unwrap().len();
        JournalReader::open(file, len, PB, ARITY).unwrap()
    }

    /// A freshly created journal with zero records must scan as an empty,
    /// valid journal — "header only" is OK.
    #[test]
    fn empty_journal_is_valid() {
        let path = tmp("empty.journal");
        JournalWriter::create(&path, 0xAAAA, 100, PB).unwrap();
        let (header, mut reader) = open_scan(&path);
        assert_eq!(header.base_digest, 0xAAAA);
        assert_eq!(header.base_block, 100);
        assert!(reader.next().is_none());
        assert_eq!(reader.stop(), Some(&ScanStop::Eof));
        assert_eq!(reader.valid_end_offset(), HEADER_LEN);
        assert_eq!(reader.last_valid_height(), 100);
        std::fs::remove_file(&path).unwrap();
    }

    /// Round trip through the real writer/reader: N records in, N records
    /// out, byte-identical deltas, ascending contiguous heights.
    #[test]
    fn round_trip_preserves_every_record() {
        let path = tmp("roundtrip.journal");
        let mut w = JournalWriter::create(&path, 0x1234, 10, PB).unwrap();
        for b in 11u64..=15 {
            w.append(&delta(b, b as i64), 1000 + b).unwrap();
        }
        drop(w);

        let (header, mut reader) = open_scan(&path);
        assert_eq!(header.base_block, 10);
        let mut got = Vec::new();
        for rec in reader.by_ref() {
            got.push(rec);
        }
        assert_eq!(reader.stop(), Some(&ScanStop::Eof));
        assert_eq!(got.len(), 5);
        for (i, rec) in got.iter().enumerate() {
            let b = 11 + i as u64;
            assert_eq!(rec.delta.block, b);
            assert_eq!(rec.num_items_after, 1000 + b);
        }
        assert_eq!(reader.last_valid_height(), 15);
        std::fs::remove_file(&path).unwrap();
    }

    /// Header corruption (bad magic) must be a clean `Corrupt` error, not
    /// a panic, and must not be confused with "zero records".
    #[test]
    fn header_corruption_is_rejected() {
        let path = tmp("badheader.journal");
        JournalWriter::create(&path, 1, 1, PB).unwrap();
        let mut bytes = std::fs::read(&path).unwrap();
        bytes[0] ^= 0xFF;
        std::fs::remove_file(&path).unwrap();
        let len = bytes.len() as u64;
        match JournalReader::open(Cursor::new(bytes), len, PB, ARITY) {
            Err(JournalError::Corrupt(msg)) => assert!(msg.contains("magic")),
            other => panic!("expected a Corrupt(magic) rejection, got {other:?}"),
        }
    }

    /// A header xxh3 mismatch (a flipped bit inside the committed
    /// `base_block`, not just the magic) must also be rejected before any
    /// record is trusted.
    #[test]
    fn header_checksum_mismatch_is_rejected() {
        let path = tmp("badheadersum.journal");
        JournalWriter::create(&path, 0xDEAD_BEEF, 42, PB).unwrap();
        let mut bytes = std::fs::read(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        bytes[10] ^= 0x01; // inside base_block's 8 bytes
        let len = bytes.len() as u64;
        match JournalReader::open(Cursor::new(bytes), len, PB, ARITY) {
            Err(JournalError::Corrupt(msg)) => assert!(msg.contains("checksum")),
            other => panic!("expected a Corrupt(checksum) rejection, got {other:?}"),
        }
    }

    /// Matching `base_digest` against the currently loaded base is the
    /// caller's job (this reader has no notion of "the current base") —
    /// pinned here as the documented contract: a header that parses
    /// cleanly still reports a `base_digest` a caller must compare
    /// against its own loaded state before trusting anything else from
    /// this reader. `crate::state::load_with_journal_restore` and
    /// `scan_report_only` are exactly that caller.
    #[test]
    fn base_mismatch_is_the_callers_responsibility() {
        let path = tmp("wrongbase.journal");
        JournalWriter::create(&path, 0x1111_1111, 50, PB).unwrap();
        let (header, _reader) = open_scan(&path);
        let loaded_base_digest = 0x2222_2222u64; // pretend the actual loaded base differs
        assert_ne!(
            header.base_digest, loaded_base_digest,
            "sanity: the scenario this test documents requires a real mismatch"
        );
        std::fs::remove_file(&path).unwrap();
    }

    /// A torn tail (file cut off mid-record) must stop the scan at the
    /// last good record, not error the whole read or panic.
    #[test]
    fn torn_tail_mid_record_stops_cleanly() {
        let path = tmp("torn.journal");
        let mut w = JournalWriter::create(&path, 1, 0, PB).unwrap();
        w.append(&delta(1, 3), 1).unwrap();
        w.append(&delta(2, 4), 2).unwrap();
        drop(w);

        let mut bytes = std::fs::read(&path).unwrap();
        bytes.truncate(bytes.len() - 3); // cut into the last record
        std::fs::remove_file(&path).unwrap();
        let len = bytes.len() as u64;
        let (_header, mut reader) =
            JournalReader::open(Cursor::new(bytes), len, PB, ARITY).unwrap();

        let rec1 = reader.next().expect("first record must still parse");
        assert_eq!(rec1.delta.block, 1);
        assert!(reader.next().is_none(), "torn second record must not parse");
        assert!(matches!(reader.stop(), Some(ScanStop::Invalid { .. })));
        assert_eq!(reader.last_valid_height(), 1);
    }

    /// A bit flip inside a later record must stop the scan exactly there
    /// — earlier records are still returned, and nothing after the flip
    /// is ever produced (never "skip the bad one and keep going").
    #[test]
    fn mid_file_bit_flip_stops_at_that_record() {
        let path = tmp("bitflip.journal");
        let mut w = JournalWriter::create(&path, 1, 0, PB).unwrap();
        for b in 1u64..=4 {
            w.append(&delta(b, b as i64), b).unwrap();
        }
        drop(w);

        let mut bytes = std::fs::read(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        // Flip a byte near the end of the file (inside one of the later
        // records, by construction of this fixture).
        let flip_at = bytes.len() - 20;
        bytes[flip_at] ^= 0x01;
        let len = bytes.len() as u64;
        let (_header, mut reader) =
            JournalReader::open(Cursor::new(bytes), len, PB, ARITY).unwrap();

        let mut got = Vec::new();
        for rec in reader.by_ref() {
            got.push(rec.delta.block);
        }
        assert!(matches!(reader.stop(), Some(ScanStop::Invalid { .. })));
        assert!(
            got.len() < 4,
            "the flip must have stopped the scan before all 4 records"
        );
        for (i, b) in got.iter().enumerate() {
            assert_eq!(
                *b,
                i as u64 + 1,
                "records returned before the flip must be exactly the good prefix, in order"
            );
        }
    }

    /// A height gap (a record whose block skips ahead) must stop the scan
    /// at the gap, not silently renumber or skip over it. Continuity is
    /// enforced at append time too, so a gap can only be *observed* on
    /// read after out-of-band tampering — exactly the hostile-input
    /// scenario this reader must survive.
    #[test]
    fn height_gap_stops_the_scan() {
        let path = tmp("gap.journal");
        {
            let mut w = JournalWriter::create(&path, 1, 0, PB).unwrap();
            w.append(&delta(1, 1), 1).unwrap();
        }
        // Splice in a hand-built record for block 3 (skipping 2) after
        // the real record for block 1.
        let mut bytes = std::fs::read(&path).unwrap();
        let gap_delta = delta(3, 1);
        let payload = risepir_proto::codec::encode_block_delta(&gap_delta, PB);
        let mut hasher = xxhash_rust::xxh3::Xxh3::new();
        let n_bytes = 2u64.to_le_bytes();
        hasher.update(&n_bytes);
        hasher.update(&payload);
        bytes.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&n_bytes);
        bytes.extend_from_slice(&payload);
        bytes.extend_from_slice(&hasher.digest().to_le_bytes());
        std::fs::remove_file(&path).unwrap();

        let len = bytes.len() as u64;
        let (_header, mut reader) =
            JournalReader::open(Cursor::new(bytes), len, PB, ARITY).unwrap();
        let rec1 = reader.next().expect("block 1 must parse");
        assert_eq!(rec1.delta.block, 1);
        assert!(reader.next().is_none(), "the gapped record must not parse");
        match reader.stop() {
            Some(ScanStop::Invalid { reason, .. }) => assert!(reason.contains("gap")),
            other => panic!("expected an Invalid(gap) stop, got {other:?}"),
        }
    }

    /// An oversized declared record length must be a clean stop with no
    /// allocation ever attempted from the hostile value — pins the
    /// boundary explicitly at `MAX_RECORD_BYTES` and above (the fuzz
    /// target exercises this at scale).
    #[test]
    fn oversized_len_is_rejected_without_allocating() {
        let path = tmp("oversize.journal");
        JournalWriter::create(&path, 1, 0, PB).unwrap();
        let mut bytes = std::fs::read(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        // A record header claiming u32::MAX bytes, with nothing behind
        // it at all — if the reader ever sized a `Vec` from this before
        // checking, it would OOM/abort long before this assertion.
        bytes.extend_from_slice(&u32::MAX.to_le_bytes());
        let len = bytes.len() as u64;
        let (_header, mut reader) =
            JournalReader::open(Cursor::new(bytes), len, PB, ARITY).unwrap();
        assert!(reader.next().is_none());
        match reader.stop() {
            Some(ScanStop::Invalid { reason, .. }) => assert!(reason.contains("out of bounds")),
            other => panic!("expected an Invalid rejection, got {other:?}"),
        }
    }

    /// A declared length that is *within* `MAX_RECORD_BYTES` but still
    /// exceeds what is actually left in the file is a second, independent
    /// bound (validate every length before allocating) — distinct from
    /// the absolute `MAX_RECORD_BYTES` cap tested above.
    #[test]
    fn declared_length_exceeding_remaining_file_size_is_rejected() {
        let path = tmp("shortfile.journal");
        JournalWriter::create(&path, 1, 0, PB).unwrap();
        let mut bytes = std::fs::read(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        // A plausible-looking length (well under MAX_RECORD_BYTES) with
        // only a few real bytes behind it.
        bytes.extend_from_slice(&1000u32.to_le_bytes());
        bytes.extend_from_slice(&[0u8; 5]); // far short of num_items_after(8) + 1000 + xxh3(8)
        let len = bytes.len() as u64;
        let (_header, mut reader) =
            JournalReader::open(Cursor::new(bytes), len, PB, ARITY).unwrap();
        assert!(reader.next().is_none());
        match reader.stop() {
            Some(ScanStop::Invalid { reason, .. }) => {
                assert!(reason.contains("remaining file size"))
            }
            other => panic!("expected an Invalid rejection, got {other:?}"),
        }
    }

    /// `append` refuses (and writes nothing for) a non-contiguous block —
    /// the in-process twin of the on-disk height-gap check above.
    #[test]
    fn append_refuses_a_gap() {
        let path = tmp("appendgap.journal");
        let mut w = JournalWriter::create(&path, 1, 0, PB).unwrap();
        w.append(&delta(1, 1), 1).unwrap();
        match w.append(&delta(3, 1), 2) {
            Err(JournalError::Gap {
                expected: 2,
                found: 3,
            }) => {}
            other => panic!("expected Gap{{expected:2,found:3}}, got {other:?}"),
        }
        assert_eq!(
            w.last(),
            1,
            "the rejected append must not have moved `last` forward"
        );
        std::fs::remove_file(&path).unwrap();
    }

    /// `adopt` truncates away a bad tail and resumes clean appending — the
    /// mechanism that turns a torn-tail scan into a usable journal again.
    #[test]
    fn adopt_truncates_bad_tail_and_resumes() {
        let path = tmp("adopt.journal");
        {
            let mut w = JournalWriter::create(&path, 1, 0, PB).unwrap();
            w.append(&delta(1, 1), 1).unwrap();
            w.append(&delta(2, 2), 2).unwrap();
        }
        let good_len = std::fs::metadata(&path).unwrap().len();
        {
            // Simulate a torn write: garbage appended directly.
            let mut f = OpenOptions::new().append(true).open(&path).unwrap();
            f.write_all(&[0xFFu8; 5]).unwrap();
        }

        let (header, mut reader) = open_scan(&path);
        let mut n = 0;
        while reader.next().is_some() {
            n += 1;
        }
        assert_eq!(
            n, 2,
            "both clean records must still parse before the garbage tail"
        );
        assert!(matches!(reader.stop(), Some(ScanStop::Invalid { .. })));
        assert_eq!(reader.valid_end_offset(), good_len);

        let mut w = JournalWriter::adopt(
            &path,
            PB,
            reader.valid_end_offset(),
            reader.last_valid_height(),
        )
        .unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().len(),
            good_len,
            "adopt must have truncated the garbage tail"
        );
        w.append(&delta(3, 3), 3).unwrap();

        let (_header2, mut reader2) = open_scan(&path);
        let mut heights = Vec::new();
        for rec in reader2.by_ref() {
            heights.push(rec.delta.block);
        }
        assert_eq!(heights, vec![1, 2, 3]);
        assert_eq!(reader2.stop(), Some(&ScanStop::Eof));
        assert_eq!(header.base_block, 0);
        std::fs::remove_file(&path).unwrap();
    }

    /// `scan_report_only`'s three "nothing to report" cases (absent file,
    /// corrupt header, mismatched base) must all fold into `Ok(None)` —
    /// the caller treats them identically (proceed as if there were no
    /// journal, no alarming log).
    #[test]
    fn scan_report_only_folds_absent_corrupt_and_mismatched_into_none() {
        let missing = tmp("does-not-exist.journal");
        assert_eq!(
            scan_report_only(&missing, 1, PB, ARITY)
                .unwrap()
                .map(|_| ()),
            None
        );

        let corrupt = tmp("corrupt-header.journal");
        JournalWriter::create(&corrupt, 1, 0, PB).unwrap();
        let mut bytes = std::fs::read(&corrupt).unwrap();
        bytes[0] ^= 0xFF;
        std::fs::write(&corrupt, &bytes).unwrap();
        assert_eq!(
            scan_report_only(&corrupt, 1, PB, ARITY)
                .unwrap()
                .map(|_| ()),
            None
        );
        std::fs::remove_file(&corrupt).unwrap();

        let mismatched = tmp("mismatched-base.journal");
        JournalWriter::create(&mismatched, 0xAAA, 0, PB).unwrap();
        assert_eq!(
            scan_report_only(&mismatched, 0xBBB, PB, ARITY)
                .unwrap()
                .map(|_| ()),
            None
        );
        std::fs::remove_file(&mismatched).unwrap();
    }

    /// A matching journal reports its true record count and end height.
    #[test]
    fn scan_report_only_reports_a_matching_journal() {
        let path = tmp("matching.journal");
        let mut w = JournalWriter::create(&path, 0x777, 5, PB).unwrap();
        for b in 6u64..=8 {
            w.append(&delta(b, 2), b).unwrap();
        }
        drop(w);

        let report = scan_report_only(&path, 0x777, PB, ARITY)
            .unwrap()
            .expect("must match");
        assert_eq!(report.count, 3);
        assert_eq!(report.end_height, 8);
        assert_eq!(report.stop, ScanStop::Eof);
        std::fs::remove_file(&path).unwrap();
    }
}
