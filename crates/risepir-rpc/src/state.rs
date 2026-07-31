//! Server state persistence: one file holding everything
//! [`RisePirServer::from_parts`] needs to reassemble a mainnet deployment
//! after a restart — cells, per-segment `A` params + hints, block, item
//! count, codec widths, and whether the account set is *complete*
//! (snapshot-bootstrapped) or *partial* (`docs/deploy.md`).
//!
//! # Format (all integers little-endian)
//!
//! ```text
//! magic  b"RPST3"
//! u8     complete            (1 = complete nonzero set; 0 = partial)
//! u32×3  key_tag_bits, balance_bits, checksum_bits
//! u64    num_items
//! u64    setup_len,  then setup_len bytes   (risepir_http::wire::encode_setup)
//! u64    cells_len,  then cells_len × u32   (the store's flat cell array)
//! u64    xxh3-64 of every preceding byte    (v2 only)
//! ```
//!
//! The setup section reuses the HTTP transport's own `GET /setup` codec —
//! one codec, one set of length-validation rules, no second serializer to
//! drift. Writes go to `<path>.tmp` then rename, so a crash mid-save
//! never truncates the previous good state.
//!
//! # Why the trailing checksum (v2)
//!
//! v1's structural checks (magic, codec widths, geometry-exact cell
//! count, trailing-bytes probe, store reconstruction) catch truncation
//! and format drift, but **not a bit flip inside the cells**: a flipped
//! bit in a slot's *fingerprint* region makes the candidate-bucket scan
//! miss and the account silently reads `0x0` — the exact failure class
//! the repo's first rule forbids, delivered by a disk. The whole-file
//! xxh3 turns any storage-layer corruption into a loud
//! [`StateError::Corrupt`] at load.
//!
//! Legacy `RPST1`/`RPST2` files **no longer load**. They used to (with a
//! stderr warning) so existing deployments upgraded on their next save,
//! but the primitive's item hash has since moved `xxh3_64` -> `xxh3_128`,
//! which re-places every key while leaving the entire header byte-identical
//! — so an old file is not stale, it is *wrong*, in the one way no cheaper
//! check can see. The format version carries that lineage now: see the
//! magic match in `parse_raw`.
//!
//! # Scale note
//!
//! At mainnet-complete scale the cells section is ~10 GB; save/load are
//! streamed through fixed 64 Ki-cell chunks, never materialising a second
//! byte-copy of the array.

use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::Path;
use std::time::{Duration, Instant};

use ikpir_common::{HintPatchMode, IncrementalPirBackend, IndexPirBackend, SimplePirBackend};
use risepir_http::wire;
use risepir_proto::{Backend, BlockDelta, Geometry, ValueCodec};
use risepir_server::{RisePirServer, SetupBundle};
use segmented_cuckoo::{CuckooParams, Segmented2aryCuckooKVStore, Segmented2aryScheme};

use crate::journal::{self, JournalReader, ScanStop};

const MAGIC_V1: &[u8; 5] = b"RPST1";
/// Superseded by [`MAGIC`] when the pinned primitive's item hash changed
/// from `xxh3_64` to `xxh3_128` — see [`parse_raw`]'s magic match for why
/// that has to invalidate every previously written file.
const MAGIC_V2: &[u8; 5] = b"RPST2";
const MAGIC: &[u8; 5] = b"RPST3";
/// Cells per streamed chunk (256 KiB of bytes at 4 B/cell).
const CHUNK_CELLS: usize = 64 * 1024;

/// The concrete server type this deployment persists.
pub type Server = RisePirServer<Segmented2aryScheme, SimplePirBackend>;

/// Arity of the concrete store type [`Server`] is compiled against
/// (`Segmented2aryScheme` / `Segmented2aryCuckooKVStore`, ADR-0034). There
/// is no associated const to read this off `Server` itself —
/// `IndexScheme::arity` is an instance method, and the scheme types carry a
/// `segment_size` field rather than a compile-time arity — so this is a
/// hand-kept mirror, pinned against a real instance of the compiled store
/// type by `store_arity_matches_the_compiled_store_type` in the tests
/// below. [`parse_raw`] compares a loaded state file's own geometry
/// against this *by name*, milliseconds after the header decodes, rather
/// than letting a mismatch surface only after the full cells array (~36 GB
/// at the complete mainnet set) has already been read and allocated, or
/// only as [`assemble`]'s `from_cells` rejection deep inside store
/// reconstruction — both of which are also true, but too late and too easy
/// to misread as disk corruption rather than a stale binary/state pairing.
const STORE_ARITY: usize = 2;

/// Everything [`load`] returns: the reassembled server plus the
/// completeness marker the JSON-RPC front end's `NotFound` policy is
/// keyed on (complete ⇒ `0x0`; partial ⇒ error — `docs/plan.md`
/// ADR-0015 only licenses `0x0`-by-absence for a *complete* set).
pub struct LoadedState {
    /// The reassembled server, bit-identical `A`/hints to the run that
    /// saved it.
    pub server: Server,
    /// Whether the persisted set was complete (snapshot-bootstrapped).
    pub complete: bool,
    /// The file's trailing whole-file xxh3 digest — always `Some` now that
    /// only `RPST3` loads (the verified checksum every current save
    /// produces); the `None` case existed for legacy `RPST1` files, which
    /// are refused outright since the `xxh3_128` switch. A delta
    /// journal (`docs/adr/README.md` ADR-0026) binds itself to this
    /// digest, so `None` here means any journal sitting beside this file
    /// is unusable until the next save upgrades it to `RPST2`.
    pub digest: Option<u64>,
}

/// Errors from [`save`] / [`load`]. Strings carry enough context to
/// diagnose a bad file without a debugger; a state file that fails *any*
/// check is rejected outright — restarting from a corrupt state and
/// serving from it would be a wrong answer waiting to happen.
#[derive(Debug)]
pub enum StateError {
    /// File open/read/write/rename failure.
    Io(String),
    /// The file is not a well-formed `RPST1` state file, or its contents
    /// failed a consistency check (codec mismatch, cell-count mismatch,
    /// store reconstruction rejection).
    Corrupt(String),
}

impl std::fmt::Display for StateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(m) => write!(f, "state file I/O: {m}"),
            Self::Corrupt(m) => write!(f, "state file rejected: {m}"),
        }
    }
}

impl std::error::Error for StateError {}

fn io_err(e: impl std::fmt::Display) -> StateError {
    StateError::Io(e.to_string())
}

/// `Write` adapter that folds every byte written through it into an xxh3
/// state, so [`save`] computes the trailing checksum in the same streamed
/// pass that writes the file (never a second copy of the ~10 GB cells).
/// Also counts bytes, so [`save`] can report the file size it produced
/// without a second `stat`.
struct HashingWriter<W: Write> {
    inner: W,
    hasher: xxhash_rust::xxh3::Xxh3,
    written: u64,
}

impl<W: Write> Write for HashingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.hasher.update(&buf[..n]);
        self.written += n as u64;
        Ok(n)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// `Read` twin of [`HashingWriter`]: everything read through it is folded
/// into the hash, so [`load`] verifies in its single streamed pass. The
/// trailing checksum itself is read via [`Self::read_unhashed`] so it is
/// excluded from its own coverage.
struct HashingReader<R: Read> {
    inner: R,
    hasher: xxhash_rust::xxh3::Xxh3,
}

impl<R: Read> HashingReader<R> {
    fn read_unhashed(&mut self, buf: &mut [u8]) -> std::io::Result<()> {
        self.inner.read_exact(buf)
    }
}

impl<R: Read> Read for HashingReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.hasher.update(&buf[..n]);
        Ok(n)
    }
}

/// What [`save`] produced: the file's total byte size and its trailing
/// `RPST2` checksum. The checksum is what
/// [`crate::journal::JournalWriter::create`] binds a fresh journal to at
/// rotation (`docs/adr/README.md` ADR-0026), so a journal can never
/// silently extend the wrong base file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SaveReport {
    /// Total byte size of the file written.
    pub bytes: u64,
    /// The trailing whole-file xxh3 digest [`save`] computed while
    /// streaming the write — identical to what a subsequent [`load`]
    /// would verify.
    pub digest: u64,
}

/// Serialize `server` (+ the completeness marker) to `path`, atomically
/// (`<path>.tmp` + rename, with the data fsynced *before* the rename so a
/// power cut cannot replace the previous good file with one whose bytes
/// never reached the disk). Always writes the current (`RPST2`,
/// checksummed) format.
pub fn save(server: &Server, codec: &ValueCodec, complete: bool, path: &Path) -> Result<SaveReport, StateError> {
    let tmp = path.with_extension("tmp");
    let (total, digest) = {
        let file = File::create(&tmp).map_err(io_err)?;
        let mut w = HashingWriter {
            inner: BufWriter::new(file),
            hasher: xxhash_rust::xxh3::Xxh3::new(),
            written: 0,
        };

        w.write_all(MAGIC).map_err(io_err)?;
        w.write_all(&[u8::from(complete)]).map_err(io_err)?;
        for width in [codec.key_tag_bits, codec.balance_bits, codec.checksum_bits] {
            w.write_all(&width.to_le_bytes()).map_err(io_err)?;
        }
        w.write_all(&server.num_items().to_le_bytes()).map_err(io_err)?;

        let setup_bytes = wire::encode_setup(&server.setup());
        w.write_all(&(setup_bytes.len() as u64).to_le_bytes()).map_err(io_err)?;
        w.write_all(&setup_bytes).map_err(io_err)?;

        // Borrowed, never `snapshot_cells()`: at the complete mainnet set
        // the cell array is ~35 GB, so copying it here would double peak
        // RSS at exactly the moment the process is already at its largest.
        let cells = server.cells();
        w.write_all(&(cells.len() as u64).to_le_bytes()).map_err(io_err)?;
        let mut chunk = Vec::with_capacity(CHUNK_CELLS * 4);
        for block in cells.chunks(CHUNK_CELLS) {
            chunk.clear();
            for c in block {
                chunk.extend_from_slice(&c.to_le_bytes());
            }
            w.write_all(&chunk).map_err(io_err)?;
        }

        // Trailing checksum of everything above — written raw to the
        // inner writer (it must not fold into itself).
        let digest = w.hasher.digest();
        w.inner.write_all(&digest.to_le_bytes()).map_err(io_err)?;
        // Flush + fsync before the rename: rename is the commit point, and
        // committing data the disk has not durably accepted would let a
        // power cut destroy the *previous* good state file too. (A plain
        // process kill never needed this — the page cache survives it —
        // but the autosave path renames dozens of times a day, so the
        // rename-before-data window is no longer a once-per-deployment
        // lottery ticket.)
        let file = w.inner.into_inner().map_err(io_err)?;
        file.sync_all().map_err(io_err)?;
        (w.written + 8, digest)
    };
    std::fs::rename(&tmp, path).map_err(io_err)?;
    fsync_parent_dir(path);
    Ok(SaveReport { bytes: total, digest })
}

/// Best-effort fsync of `path`'s parent directory, for after a rename:
/// the data fsync above makes the *file* durable, but the rename itself
/// lives in the directory, and a power cut before the directory entry
/// reaches disk rolls back to the previous save. Best-effort — an error
/// (some filesystems refuse `fsync` on a directory handle) costs exactly
/// that already-documented staleness window, never correctness, and must
/// not turn every save into a failure on such a filesystem.
pub(crate) fn fsync_parent_dir(path: &Path) {
    let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) else {
        return;
    };
    if let Ok(dir) = File::open(parent) {
        let _ = dir.sync_all();
    }
}

/// Claims `path` as this process's `--state` path, for the process
/// lifetime. Two guards, both against operator accidents rather than
/// attackers:
///
/// 1. **Sibling-suffix self-collisions.** This module derives sibling
///    paths via `with_extension` — `.journal` (rotation), `.tmp` (save
///    staging), `.lock` (this function) — and `crate::snapshot_audit`
///    derives a fourth, `.audit` (the post-bootstrap snapshot audit
///    sidecar, ADR-0040) — so a `--state` path *already* ending in one of
///    those derives a sibling equal to itself: rotation would rename a
///    header-only journal **over the state file just saved**, a save
///    would stage into its own target, the lock's `File::create` would
///    truncate the state file outright, and an audit write would replace
///    it with a few lines of `key=value` text. Absurd inputs, one refusal
///    for the family.
///
/// 2. **A second process on the same path.** Both would stage into the
///    same `<path>.tmp` and interleave writes; whichever renames last
///    installs a torn file. The trailing checksum catches that loudly at
///    the *next* load — but only after the good file was already
///    destroyed. An advisory `try_lock` on a `<path>.lock` sidecar makes
///    the second process fail fast at startup instead. The returned
///    `File` is the lock: hold it for as long as the path is in use
///    (dropping it releases the claim).
///
/// The lock file itself stays behind (empty) after exit — advisory locks
/// die with the process, so a stale file is harmless and never blocks a
/// restart.
pub fn acquire_state_path(path: &Path) -> Result<File, StateError> {
    // Every sibling this module (or `crate::snapshot_audit`) derives via
    // `with_extension` collides with the state path itself when the path
    // already carries that extension: `.journal` (rotation would rename a
    // header-only journal over the state file), `.tmp` (the save would
    // stage *into* its own target, voiding the previous-good-file
    // atomicity), `.lock` (the `File::create` below would truncate the
    // state file), `.audit` (the snapshot-audit sidecar, ADR-0040, would
    // overwrite the state file with a few lines of text). One check
    // covers the whole family.
    for reserved in ["journal", "tmp", "lock", "audit"] {
        if path.extension().and_then(|e| e.to_str()) == Some(reserved) {
            return Err(StateError::Io(format!(
                "--state {} ends in .{reserved}, which this server derives as a sibling file's \
                 suffix for that same path — the two would collide and destroy the state file; \
                 pick any other extension (.bin is conventional)",
                path.display()
            )));
        }
    }
    let lock_path = path.with_extension("lock");
    let lock = File::create(&lock_path).map_err(io_err)?;
    match lock.try_lock() {
        Ok(()) => Ok(lock),
        Err(e) => Err(StateError::Io(format!(
            "another process already holds {} ({e}) — two writers on one --state path would \
             interleave writes into the same {}.tmp and destroy the good state file; stop the \
             other process first (is a tmux session still running it?)",
            lock_path.display(),
            path.display()
        ))),
    }
}

/// Crate-visible raw materials behind a loaded state file, from *before*
/// store reconstruction — what [`load_with_journal_restore`] needs to
/// mutate (cells, hints, `num_items`, `block`) prior to the store/server
/// being built from them. Not `pub`: nothing outside this crate needs a
/// state file's insides this raw; [`load`] / [`load_bytes`] still return
/// the fully assembled [`LoadedState`] they always have.
pub(crate) struct RawState {
    pub(crate) cells: Vec<u32>,
    pub(crate) setup: SetupBundle<SimplePirBackend>,
    pub(crate) num_items: u64,
    pub(crate) complete: bool,
    pub(crate) digest: Option<u64>,
}

/// Read a state file back into a running server. `config` must be the
/// same backend config the deployment always uses (it only matters for a
/// later `full_rebuild`); `codec` must match the widths recorded in the
/// file — a mismatch means the operator changed the value encoding
/// between runs, which silently breaks every stored slot, so it is
/// rejected loudly instead.
pub fn load(path: &Path, config: ikpir_common::SimpleConfig, codec: &ValueCodec) -> Result<LoadedState, StateError> {
    let file = File::open(path).map_err(io_err)?;
    let total_len = file.metadata().map_err(io_err)?.len();
    load_from(BufReader::new(file), total_len, config, codec)
}

/// [`load`] over an in-memory byte slice — what the state-file fuzz
/// target drives, and handy in tests. Identical validation to [`load`].
pub fn load_bytes(bytes: &[u8], config: ikpir_common::SimpleConfig, codec: &ValueCodec) -> Result<LoadedState, StateError> {
    load_from(bytes, bytes.len() as u64, config, codec)
}

/// Crate-visible twin of [`load`] that stops short of store
/// reconstruction — the entry point [`load_with_journal_restore`] uses so
/// it can mutate the raw cells/hints before any store exists.
pub(crate) fn load_raw(path: &Path, codec: &ValueCodec) -> Result<RawState, StateError> {
    let file = File::open(path).map_err(io_err)?;
    let total_len = file.metadata().map_err(io_err)?.len();
    parse_raw(BufReader::new(file), total_len, codec)
}

/// Rejects a state file whose stored geometry is not the one this binary
/// would build (ADR-0042).
///
/// [`parse_raw`] already refuses a mismatched **arity** ([`STORE_ARITY`],
/// ADR-0034) and a mismatched **[`ValueCodec`]** field-by-field from the
/// header. This closes the rest: `bucket_size`, `fingerprint_bits` and
/// `plaintext_bits` all ride in the file's own setup bundle, and until this
/// check existed they were only validated for self-consistency against
/// `cells_len` — so a file written at one `(bucket_size, f, pb)` loaded
/// happily into a binary compiled for another.
///
/// That is **not** a wrong-answer bug: the store, `GET /setup` and every
/// client all take the geometry from the file, so the deployment stays
/// internally consistent. It is a *silent wrong operating point* — the same
/// class as ADR-0034's arity trap and the partial-state-file trap, and the
/// exact mode by which a geometry decision can be believed-deployed and not
/// be. Erroring is fine; silence is not.
///
/// Two things this deliberately does not do:
///
/// - **It does not pin `num_buckets`.** That legitimately varies with
///   `--partial-capacity`, so the expected geometry is derived *at the
///   file's own* `num_buckets` — the guard asks "would this binary have
///   built this geometry at this size?", not "is this the deployment size?".
/// - **It does not hardcode `plaintext_bits`.** The expectation comes from
///   [`Geometry::for_num_buckets`], the same selector
///   `Geometry::for_accounts` uses on the bootstrap path, so if the pinned
///   primitive ever retunes that selector this guard tracks it instead of
///   going stale. That is the case worth guarding against: a `pb` that
///   moves under a binary nobody edited.
fn check_geometry_lineage(params: &CuckooParams, codec: &ValueCodec) -> Result<(), StateError> {
    let expected = Geometry::for_num_buckets(
        params.num_buckets,
        STORE_ARITY as u32,
        crate::mainnet::BUCKET_SIZE,
        crate::mainnet::FINGERPRINT_BITS,
        codec,
        Backend::Simple,
    )
    .map_err(|e| {
        StateError::Corrupt(format!(
            "state file declares num_buckets {}, which is not a geometry this binary can \
             describe: {e}",
            params.num_buckets
        ))
    })?;

    if params.bucket_size == expected.bucket_size
        && params.fingerprint_bits == expected.fingerprint_bits
        && params.plaintext_bits == expected.plaintext_bits
    {
        return Ok(());
    }
    Err(StateError::Corrupt(format!(
        "state file geometry is (bucket_size {}, fingerprint_bits {}, plaintext_bits {}) but \
         this binary derives (bucket_size {}, fingerprint_bits {}, plaintext_bits {}) at that \
         file's own num_buckets ({}) — this is not disk corruption, it is an intact state file \
         from a previous geometry lineage (ADR-0042); move the --state file aside and \
         re-bootstrap from a fresh snapshot (do not restore from backup, the file itself is fine)",
        params.bucket_size,
        params.fingerprint_bits,
        params.plaintext_bits,
        expected.bucket_size,
        expected.fingerprint_bits,
        expected.plaintext_bits,
        params.num_buckets,
    )))
}

/// The single decode path behind [`load`] / [`load_bytes`] / [`load_raw`].
/// `total_len` is the input's real size, used to bound every
/// header-declared length *before* it sizes an allocation — a corrupt or
/// hostile file must produce a clean [`StateError`], never an OOM (the
/// repo's validate-every-length rule applies to state files too, not just
/// the network). Also rejects a state file of the wrong arity lineage (see
/// [`STORE_ARITY`]) or of any other mismatched geometry (see
/// [`check_geometry_lineage`]) right after the header decodes, before the
/// cells section is even read, let alone allocated. Stops short of store
/// reconstruction — see [`RawState`] and [`assemble`].
fn parse_raw(reader: impl Read, total_len: u64, codec: &ValueCodec) -> Result<RawState, StateError> {
    let mut r = HashingReader {
        inner: reader,
        hasher: xxhash_rust::xxh3::Xxh3::new(),
    };

    let mut magic = [0u8; 5];
    r.read_exact(&mut magic).map_err(io_err)?;
    match &magic {
        m if m == MAGIC => {}
        // The one geometry-invisible invalidation. When the primitive's item
        // hash moved `xxh3_64` -> `xxh3_128` (IKPIR `0f3b99b`), the primary
        // bucket index and the fingerprint started coming from a *different*
        // digest, so every key now lands somewhere else. Nothing in the
        // header changes: arity, `ValueCodec`, `bucket_size`,
        // `fingerprint_bits`, `plaintext_bits` and `num_buckets` are all
        // byte-identical across the switch, so neither `STORE_ARITY` nor
        // `check_geometry_lineage` can see it — and an old file would load
        // clean and then miss on every lookup, answering `0x0` for accounts
        // that exist (a complete set treats absence as zero, ADR-0015). That
        // is the silent wrong answer this repo forbids outright, so the file
        // format's own version is what has to carry the lineage: `RPST3` is
        // written only by `xxh3_128` builds, and `RPST1`/`RPST2` are refused
        // here, before a single cell is read.
        m if m == MAGIC_V1 || m == MAGIC_V2 => {
            let version = if m == MAGIC_V1 { "RPST1" } else { "RPST2" };
            return Err(StateError::Corrupt(format!(
                "state file is {version}, written before the primitive's item hash changed from \
                 xxh3_64 to xxh3_128 — every key hashes to a different bucket now, so these cells \
                 no longer describe a filter this binary can read, and loading them would miss on \
                 every lookup (answering 0x0 for accounts that exist). This is not disk \
                 corruption, and the geometry is unchanged, which is exactly why nothing cheaper \
                 catches it; move the --state file aside and re-bootstrap from a fresh snapshot \
                 (do not restore from backup, the file itself is fine)"
            )));
        }
        _ => {
            return Err(StateError::Corrupt(
                "bad magic (not an RPST1/RPST2/RPST3 state file)".to_string(),
            ));
        }
    };
    let mut flag = [0u8; 1];
    r.read_exact(&mut flag).map_err(io_err)?;
    let complete = match flag[0] {
        0 => false,
        1 => true,
        other => return Err(StateError::Corrupt(format!("bad completeness flag {other}"))),
    };

    let mut widths = [0u32; 3];
    for w in &mut widths {
        *w = read_u32(&mut r)?;
    }
    let file_codec = ValueCodec {
        key_tag_bits: widths[0],
        balance_bits: widths[1],
        checksum_bits: widths[2],
    };
    if file_codec != *codec {
        return Err(StateError::Corrupt(format!(
            "value codec mismatch: file has key_tag/balance/checksum = {}/{}/{} bits, \
             deployment configured {}/{}/{}",
            widths[0], widths[1], widths[2], codec.key_tag_bits, codec.balance_bits, codec.checksum_bits
        )));
    }

    let num_items = read_u64(&mut r)?;

    let setup_len = read_u64(&mut r)?;
    // Bound by the input's real size before allocating: a header claiming
    // more bytes than the file holds is corruption, not an allocation.
    if setup_len > total_len {
        return Err(StateError::Corrupt(format!(
            "setup_len {setup_len} exceeds the file's own size ({total_len} bytes)"
        )));
    }
    let setup_len = usize::try_from(setup_len).map_err(|_| StateError::Corrupt("setup_len overflow".to_string()))?;
    let mut setup_bytes = vec![0u8; setup_len];
    r.read_exact(&mut setup_bytes).map_err(io_err)?;
    let setup = wire::decode_setup(&setup_bytes).map_err(|e| StateError::Corrupt(format!("setup section: {e}")))?;
    let params = setup.params;
    if params.arity() != STORE_ARITY {
        return Err(StateError::Corrupt(format!(
            "state file geometry is arity {} but this binary is compiled for arity {STORE_ARITY} \
             (ADR-0034) — this is not disk corruption, it is an intact state file from a previous \
             geometry lineage; move the --state file aside and re-bootstrap from a fresh snapshot \
             (do not restore from backup, the file itself is fine)",
            params.arity(),
        )));
    }
    check_geometry_lineage(&params, codec)?;

    let cells_len = read_u64(&mut r)?;
    let cells_len = usize::try_from(cells_len).map_err(|_| StateError::Corrupt("cells_len overflow".to_string()))?;
    let expected =
        params.num_buckets as usize * params.bucket_size as usize * params.cells_per_slot() as usize;
    if cells_len != expected {
        return Err(StateError::Corrupt(format!(
            "cells_len {cells_len} does not match the setup section's geometry ({expected} cells)"
        )));
    }
    let mut cells = vec![0u32; cells_len];
    let mut buf = vec![0u8; CHUNK_CELLS * 4];
    let mut filled = 0usize;
    while filled < cells_len {
        let take = (cells_len - filled).min(CHUNK_CELLS);
        let bytes = &mut buf[..take * 4];
        r.read_exact(bytes).map_err(io_err)?;
        for (i, four) in bytes.chunks_exact(4).enumerate() {
            cells[filled + i] = u32::from_le_bytes([four[0], four[1], four[2], four[3]]);
        }
        filled += take;
    }

    // The whole-file xxh3 covers every byte read so far. Read the stored
    // digest *unhashed* (it cannot cover itself) and compare.
    //
    // Unconditional since `RPST1`/`RPST2` stopped loading (see the magic
    // match): every file this binary accepts carries the checksum, so the
    // `None` arm this used to have is unreachable. `digest` stays an
    // `Option<u64>` rather than a `u64` only to keep the journal's existing
    // "no digest to bind to" handling compiling; that arm is now dead and
    // can be collapsed whenever someone wants the wider change.
    let digest = {
        let computed = r.hasher.digest();
        let mut stored = [0u8; 8];
        r.read_unhashed(&mut stored).map_err(io_err)?;
        let stored = u64::from_le_bytes(stored);
        if computed != stored {
            return Err(StateError::Corrupt(format!(
                "whole-file checksum mismatch (stored {stored:#018x}, computed {computed:#018x}) — \
                 the file is corrupt; restore from backup or re-bootstrap"
            )));
        }
        Some(computed)
    };

    // Anything after the cells (+ v2 checksum) is corruption, same
    // posture as the wire codecs' TrailingBytes.
    let mut probe = [0u8; 1];
    match r.read(&mut probe) {
        Ok(0) => {}
        Ok(_) => return Err(StateError::Corrupt("trailing bytes after cells section".to_string())),
        Err(e) => return Err(io_err(e)),
    }

    Ok(RawState {
        cells,
        setup,
        num_items,
        complete,
        digest,
    })
}

/// Assembles a [`LoadedState`] from [`RawState`]'s parts: reconstructs the
/// store via `Segmented2aryCuckooKVStore::from_cells` and the server via
/// [`Server::from_parts`]. Shared by the plain load path ([`load_from`])
/// and the journal-restore path ([`load_with_journal_restore`]), which
/// mutates `cells` / `setup.hints` / `num_items` / `setup.block` in place
/// before calling this.
///
/// `from_cells` itself also rejects a `scheme_kind` that does not match
/// `Segmented2aryScheme` (`InvalidParams("scheme_kind mismatch: ...")`,
/// which would surface here as `StateError::Corrupt("store reconstruction:
/// ...")`) — but by the time either caller reaches this function,
/// [`parse_raw`]'s [`STORE_ARITY`] check has already refused a mismatched
/// state file, in milliseconds and before the cells array was even
/// allocated. This is defense in depth, not the primary guard (ADR-0034).
pub(crate) fn assemble(raw: RawState, config: ikpir_common::SimpleConfig, codec: ValueCodec) -> Result<LoadedState, StateError> {
    let store = Segmented2aryCuckooKVStore::from_cells(raw.cells, raw.setup.params, raw.num_items)
        .map_err(|e| StateError::Corrupt(format!("store reconstruction: {e:?}")))?;
    let server = Server::from_parts(store, config, codec, raw.setup.backend_params, raw.setup.hints, raw.setup.block);
    Ok(LoadedState {
        server,
        complete: raw.complete,
        digest: raw.digest,
    })
}

fn load_from(reader: impl Read, total_len: u64, config: ikpir_common::SimpleConfig, codec: &ValueCodec) -> Result<LoadedState, StateError> {
    let raw = parse_raw(reader, total_len, codec)?;
    assemble(raw, config, *codec)
}

/// Errors from [`load_with_journal_restore`] — distinguishes the two
/// failure classes `docs/adr/README.md` ADR-0026 requires.
#[derive(Debug)]
pub enum RestoreError {
    /// The base state file itself failed to load — identical to a plain
    /// [`load`] failure; the journal was never reached.
    State(StateError),
    /// Every *pre-apply* check on a journal record passed (it decoded
    /// cleanly, its checksum matched, its height was contiguous) but
    /// actually **applying** it — the `0 <= cell + delta < p` bound, or a
    /// geometry mismatch this base's segments cannot even index — failed.
    /// Deliberately not treated as "stop and use the good prefix": the
    /// in-memory cells this replay was mutating are now torn mid-block,
    /// and there is no good prefix to fall back to from inside this
    /// function. The caller (`mainnet.rs`) runs this before serving
    /// starts, so refusing to proceed at all is the honest response —
    /// never serve from it (ADR-0026 rule 3).
    ApplyFailure(String),
}

impl std::fmt::Display for RestoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            // `State` can originate either from the base file itself
            // (before the journal is even opened — `--no-journal-restore`
            // would hit the identical failure) or from `assemble`
            // *after* a structurally-valid journal has been replayed onto
            // it (where `--no-journal-restore` genuinely is the fix,
            // ADR-0037). This function cannot tell those apart from here,
            // but naming the flag costs an operator nothing in the first
            // case (the same error recurs, now unambiguously pointing at
            // the base file) and fixes the second, so it is named either
            // way.
            Self::State(e) => write!(
                f,
                "{e} — if this happened while --journal-restore was replaying the journal onto the \
                 base, restart with --no-journal-restore to load the base state file alone, without \
                 the journal, and confirm whether the base file or the journal is at fault"
            ),
            Self::ApplyFailure(m) => write!(
                f,
                "journal replay apply-time failure: {m} — the journal is the suspect, not the base \
                 file; restart with --no-journal-restore to load the base state file alone (the \
                 journal is left on disk, untouched, for inspection)"
            ),
        }
    }
}

impl std::error::Error for RestoreError {}

impl From<StateError> for RestoreError {
    fn from(e: StateError) -> Self {
        Self::State(e)
    }
}

/// What a successful [`load_with_journal_restore`] produced, beyond the
/// assembled [`LoadedState`] itself.
pub struct RestoredState {
    /// The assembled server: at the journal's final replayed height, or
    /// the base's own height if nothing was replayed.
    pub loaded: LoadedState,
    /// How many journal records were successfully replayed (`0` if there
    /// was no journal, it did not match this base, or its header was
    /// corrupt — all of which degrade gracefully to the base alone).
    pub replayed: u64,
    /// The base height *before* replay — for the startup log line's
    /// "(base was B)".
    pub base_block: u64,
    /// The most recent replayed deltas, oldest first, capped at the
    /// caller's `ring_capacity` — exactly what
    /// [`risepir_http::NodeState::seed_history`] needs so `GET /delta` and
    /// `GET /sync` cover the replayed tail immediately.
    pub tail_deltas: Vec<BlockDelta>,
    /// `Some((valid_end_offset, last_valid_height))` a fresh
    /// [`crate::journal::JournalWriter::adopt`] call should resume from —
    /// `None` when no journal was actually consulted (nothing to adopt;
    /// the next save's rotation starts a fresh one).
    pub adopt_at: Option<(u64, u64)>,
    /// Why the journal scan stopped, if a journal was consulted at all.
    pub scan_stop: Option<ScanStop>,
    /// Wall-clock time spent actually replaying records onto the raw
    /// cells/hints (ADR-0037) — deliberately excludes both the base
    /// file's own read (`load_raw`, above; often the dominant cost at the
    /// complete mainnet set, and this change does nothing to shrink it)
    /// and the final store-assembly step, so the number the startup log's
    /// "journal replayed ... in Ns" line reports is never inflated by
    /// costs the journal itself has no bearing on. `Duration::ZERO` when
    /// no journal was consulted (mirrors [`Self::replayed`] being `0` in
    /// that case).
    pub replay_elapsed: Duration,
}

/// Applies one block's per-segment cell deltas directly to a flat cell
/// array, enforcing `0 <= cell + delta < p` per cell (`p = 2^plaintext_bits`)
/// — the same bound `docs/adr/README.md` ADR-0005 licenses for the wire
/// codec, checked here because replay mutates cells directly rather than
/// going through the store's own key-addressed ops.
fn apply_delta_in_place(cells: &mut [u32], params: &CuckooParams, delta: &BlockDelta) -> Result<(), String> {
    let segment_size = params.segment_size();
    let row_width = params.bucket_size * params.cells_per_slot();
    let seg_cells = segment_size as usize * row_width as usize;
    let modulus: i64 = 1i64 << params.plaintext_bits;

    for (j, seg_deltas) in delta.per_segment.iter().enumerate() {
        if seg_deltas.is_empty() {
            continue;
        }
        let start = j * seg_cells;
        let end = start + seg_cells;
        let Some(slice) = cells.get_mut(start..end) else {
            return Err(format!("segment {j} is out of bounds for this base's geometry"));
        };
        for (row, edits) in seg_deltas {
            let row_base = *row as usize * row_width as usize;
            for (offset, cell_delta) in edits {
                // The segment-slice bound below cannot catch an offset
                // that walks past this row's end but stays inside the
                // segment — that would silently edit a *neighbouring
                // row's* cell, which is a wrong-balance channel, not a
                // crash. Only checksum-valid records from a buggy writer
                // could carry one (the codec has no row_width to check
                // against), which is exactly why replay re-checks it
                // here instead of trusting writer-side invariants.
                if u32::from(*offset) >= row_width {
                    return Err(format!(
                        "cell offset {offset} >= row width {row_width} in segment {j} row {row} — \
                         this record would edit a neighbouring row"
                    ));
                }
                let idx = row_base + *offset as usize;
                let Some(cell) = slice.get_mut(idx) else {
                    return Err(format!("cell index {idx} is out of bounds within segment {j}"));
                };
                let updated = i64::from(*cell) + *cell_delta;
                if updated < 0 || updated >= modulus {
                    return Err(format!(
                        "applying delta {cell_delta} to a cell valued {cell} in segment {j} row {row} \
                         violates 0 <= cell < 2^{} — the in-memory state is now torn mid-block",
                        params.plaintext_bits
                    ));
                }
                *cell = updated as u32;
            }
        }
    }
    Ok(())
}

/// Loads `path` and, if its journal (`crate::journal::journal_path_for(path)`)
/// exists and its header matches this base's digest, replays every valid
/// record onto the raw cells/hints *before* the store is constructed —
/// the `--journal-restore` path (`docs/adr/README.md` ADR-0026). A
/// missing/non-matching/header-corrupt journal, or a legacy (digest-less)
/// base file, degrades to exactly [`load`]'s result (`replayed: 0`,
/// `tail_deltas` empty, `adopt_at: None`) — restoring is opportunistic,
/// never a hard requirement of loading successfully.
///
/// # Errors
///
/// [`RestoreError::State`] wraps any plain load failure (identical to
/// calling [`load`] directly — the journal is never reached).
/// [`RestoreError::ApplyFailure`] if a record that passed every pre-apply
/// check (decode, checksum, height continuity) then failed to actually
/// apply — see that variant's docs; this is the one failure this function
/// cannot itself recover from, by design.
pub fn load_with_journal_restore(
    path: &Path,
    config: ikpir_common::SimpleConfig,
    codec: &ValueCodec,
    ring_capacity: usize,
) -> Result<RestoredState, RestoreError> {
    let mut raw = load_raw(path, codec)?;
    let base_block = raw.setup.block;
    let journal_path = journal::journal_path_for(path);

    let plaintext_bits = raw.setup.params.plaintext_bits;
    let arity = raw.setup.params.arity() as u32;
    // Collapses every "nothing to replay" case (legacy digest-less base,
    // missing journal, corrupt header, or a header that names a
    // different base) into one `None` — all handled identically below.
    let opened = raw.digest.and_then(|base_digest| {
        let file = File::open(&journal_path).ok()?;
        let total_len = file.metadata().ok()?.len();
        let (header, reader) = JournalReader::open(BufReader::new(file), total_len, plaintext_bits, arity).ok()?;
        if header.base_digest != base_digest {
            return None;
        }
        // Belt to the digest's braces, on the one field replay continuity
        // is actually seeded from: `header.base_block` (the reader expects
        // records from `base_block + 1`). Digest and block always come
        // from one `SaveReport` today, so a mismatch here is unreachable
        // short of a future writer bug — but that future bug's failure
        // mode would be replaying deltas onto a base that already
        // contains them, i.e. silently wrong balances, the exact class
        // ADR-0026 exists to contain. One comparison converts it to
        // "journal ignored, loudly".
        if header.base_block != base_block {
            logln!(
                "risepir-rpc: WARNING: journal {} matches this base's digest but names base block {} \
                 while the state file is at block {base_block} — refusing to replay it (writer bug?); \
                 delete the journal to silence this",
                journal_path.display(),
                header.base_block
            );
            return None;
        }
        Some(reader)
    });

    let Some(mut reader) = opened else {
        let loaded = assemble(raw, config, *codec)?;
        return Ok(RestoredState {
            loaded,
            replayed: 0,
            base_block,
            tail_deltas: Vec::new(),
            adopt_at: None,
            scan_stop: None,
            replay_elapsed: Duration::ZERO,
        });
    };

    let material: Vec<_> = raw
        .setup
        .backend_params
        .iter()
        .map(SimplePirBackend::expand_hint_material)
        .collect();

    let mut tail: std::collections::VecDeque<BlockDelta> = std::collections::VecDeque::new();
    let mut replayed = 0u64;
    // Timed separately from the base file's read above and `assemble`
    // below (ADR-0037): this is specifically what the startup log's
    // "journal replayed ... in Ns" line now reports, so that number is
    // never inflated by the (usually much larger, at the complete
    // mainnet set) cost of reading the base file — a cost this feature
    // does nothing to reduce.
    let replay_started = Instant::now();

    for rec in reader.by_ref() {
        apply_delta_in_place(&mut raw.cells, &raw.setup.params, &rec.delta).map_err(RestoreError::ApplyFailure)?;
        for (j, seg_deltas) in rec.delta.per_segment.iter().enumerate() {
            if !seg_deltas.is_empty() {
                SimplePirBackend::server_patch_hint(
                    &raw.setup.backend_params[j],
                    &material[j],
                    &mut raw.setup.hints[j],
                    seg_deltas,
                    HintPatchMode::EntryLevel,
                );
            }
        }
        raw.num_items = rec.num_items_after;
        raw.setup.block = rec.delta.block;
        replayed += 1;
        tail.push_back(rec.delta);
        while tail.len() > ring_capacity {
            tail.pop_front();
        }
    }
    let replay_elapsed = replay_started.elapsed();

    let scan_stop = reader.stop().cloned();
    let adopt_at = Some((reader.valid_end_offset(), reader.last_valid_height()));
    let loaded = assemble(raw, config, *codec)?;
    Ok(RestoredState {
        loaded,
        replayed,
        base_block,
        tail_deltas: tail.into_iter().collect(),
        adopt_at,
        scan_stop,
        replay_elapsed,
    })
}

fn read_u32(r: &mut impl Read) -> Result<u32, StateError> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b).map_err(io_err)?;
    Ok(u32::from_le_bytes(b))
}

fn read_u64(r: &mut impl Read) -> Result<u64, StateError> {
    let mut b = [0u8; 8];
    r.read_exact(&mut b).map_err(io_err)?;
    Ok(u64::from_le_bytes(b))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ikpir_common::pir_params::simple_max_plaintext_bits;
    use ikpir_common::backend::simple::SimpleParams;
    use ikpir_common::{IndexPirBackend, SimpleConfig};
    use risepir_proto::keccak256;
    use risepir_proto::BlockUpdate;

    fn codec() -> ValueCodec {
        ValueCodec {
            key_tag_bits: 32,
            balance_bits: 96,
            checksum_bits: 16,
        }
    }

    fn small_server() -> Server {
        let codec = codec();
        let num_buckets = 2 * 64;
        let pb = simple_max_plaintext_bits(2, num_buckets / 2, 4, 32, codec.value_bits(), SimpleParams::DEFAULT_SIGMA);
        let store = Segmented2aryCuckooKVStore::new(num_buckets, 4, 32, codec.value_bits(), pb).unwrap();
        Server::new(store, SimpleConfig::with_lwe_dim(256), codec, 0)
    }

    fn tmp(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("risepir-state-{}-{name}", std::process::id()))
    }

    /// Round trip: save a populated, block-advanced server; load it; the
    /// hints, A-params, block, item count, completeness, and answers to a
    /// verified read must all survive bit-exactly — and the reloaded
    /// server must keep applying blocks from where it left off.
    #[test]
    fn round_trip_preserves_everything_and_keeps_running() {
        let mut server = small_server();
        let addrs: Vec<_> = (0u8..50).map(|i| keccak256(&[i; 20])).collect();
        server
            .apply_block(&BlockUpdate {
                block: 7,
                changes: addrs.iter().enumerate().map(|(i, a)| (*a, 1_000u128 + i as u128)).collect(),
                credits: vec![(addrs[0], 5u128)],
            })
            .unwrap();

        let before = server.setup();
        let path = tmp("roundtrip.bin");
        save(&server, &codec(), true, &path).unwrap();
        let LoadedState { server: mut reloaded, complete, .. } = load(&path, SimpleConfig::with_lwe_dim(256), &codec()).unwrap();
        std::fs::remove_file(&path).unwrap();

        assert!(complete);
        assert_eq!(reloaded.block(), 7);
        assert_eq!(reloaded.num_items(), 50);
        let after = reloaded.setup();
        for (j, (b, a)) in before.hints.iter().zip(&after.hints).enumerate() {
            assert_eq!(b.data, a.data, "segment {j}: hint bytes must survive the round trip");
        }
        for (j, (b, a)) in before.backend_params.iter().zip(&after.backend_params).enumerate() {
            // A is expanded deterministically from ServerParams; equal
            // expanded material ⇒ equal A. (SimpleServerParams has no
            // PartialEq; the expanded hint material is the observable.)
            let mb = SimplePirBackend::expand_hint_material(b);
            let ma = SimplePirBackend::expand_hint_material(a);
            assert_eq!(mb.a, ma.a, "segment {j}: A must survive the round trip");
        }
        assert_eq!(reloaded.balance_of(&addrs[0]).unwrap(), Some(1_005));
        assert_eq!(reloaded.balance_of(&addrs[49]).unwrap(), Some(1_049));

        // Still a working server: apply another block on top.
        reloaded
            .apply_block(&BlockUpdate {
                block: 8,
                changes: vec![(addrs[1], 0u128)],
                credits: vec![],
            })
            .unwrap();
        assert_eq!(reloaded.balance_of(&addrs[1]).unwrap(), None);
    }

    #[test]
    fn partial_flag_survives() {
        let server = small_server();
        let path = tmp("partial.bin");
        save(&server, &codec(), false, &path).unwrap();
        let loaded = load(&path, SimpleConfig::with_lwe_dim(256), &codec()).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert!(!loaded.complete);
    }

    /// The failure class RPST2 exists for: a single flipped bit deep in
    /// the cells section passes every *structural* check (geometry-exact
    /// count, trailing probe, store reconstruction) — under v1 it loaded
    /// "successfully" and could silently read a colliding account as
    /// `0x0`. The whole-file checksum must reject it loudly instead.
    #[test]
    fn mid_cells_bit_flip_rejected_by_checksum() {
        let server = small_server();
        let path = tmp("bitflip.bin");
        save(&server, &codec(), true, &path).unwrap();

        let mut bytes = std::fs::read(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        let mid = bytes.len() / 2; // well inside the cells section
        bytes[mid] ^= 0x01;
        match load_bytes(&bytes, SimpleConfig::with_lwe_dim(256), &codec()) {
            Err(StateError::Corrupt(msg)) => assert!(msg.contains("checksum"), "unexpected rejection: {msg}"),
            Err(other) => panic!("bit flip must be a checksum rejection, got {other}"),
            Ok(_) => panic!("bit flip must not load"),
        }
    }

    /// Legacy `RPST1` and `RPST2` files are **refused**, by name.
    ///
    /// This test used to assert the opposite — that a legacy file still
    /// loads, so a deployment upgraded on its next save instead of
    /// re-bootstrapping. That was right while the only difference was the
    /// trailing checksum. It stopped being right when the pinned
    /// primitive's item hash moved `xxh3_64` -> `xxh3_128`: every key now
    /// hashes to a different bucket, so an old file's cells describe a
    /// filter this binary cannot read, and *nothing in the header says so*
    /// — arity, codec, `bucket_size`, `fingerprint_bits`, `plaintext_bits`
    /// and `num_buckets` all match. Loading one would miss on every lookup
    /// and answer `0x0` for accounts that exist.
    ///
    /// Both fixtures are byte-valid at their own version (the `RPST2` one
    /// is a real saved file, untouched apart from its magic; the `RPST1`
    /// one additionally has the 8-byte trailer stripped, which *is* the v1
    /// format), so what rejects them is the version, not their shape.
    #[test]
    fn legacy_rpst1_and_rpst2_are_refused_by_name() {
        let mut server = small_server();
        let addr = keccak256(&[9u8; 20]);
        server
            .apply_block(&BlockUpdate { block: 3, changes: vec![(addr, 777u128)], credits: vec![] })
            .unwrap();
        let path = tmp("legacy.bin");
        save(&server, &codec(), false, &path).unwrap();
        let v3 = std::fs::read(&path).unwrap();
        std::fs::remove_file(&path).unwrap();

        // Sanity: the file we are about to downgrade loads as written.
        load_bytes(&v3, SimpleConfig::with_lwe_dim(256), &codec())
            .expect("the current-format fixture must load, or this test proves nothing");

        let mut v1 = v3.clone();
        v1.truncate(v1.len() - 8);
        v1[..5].copy_from_slice(MAGIC_V1);
        let mut v2 = v3.clone();
        v2[..5].copy_from_slice(MAGIC_V2);

        for (version, bytes) in [("RPST1", v1), ("RPST2", v2)] {
            match load_bytes(&bytes, SimpleConfig::with_lwe_dim(256), &codec()) {
                Err(StateError::Corrupt(msg)) => {
                    assert!(msg.contains(version), "must name the version it found: {msg}");
                    assert!(msg.contains("xxh3_128"), "must name the cause: {msg}");
                    assert!(msg.contains("re-bootstrap"), "must name the fix: {msg}");
                    assert!(
                        msg.contains("do not restore from backup"),
                        "the file is intact, not corrupt — must not steer to a backup: {msg}"
                    );
                    assert!(
                        !msg.contains("checksum mismatch"),
                        "must be refused for its lineage, not misreported as corruption: {msg}"
                    );
                }
                Err(StateError::Io(msg)) => panic!("{version} must be rejected as Corrupt, not Io: {msg}"),
                Ok(_) => panic!("a {version} state file predates the xxh3_128 switch and must not load"),
            }
        }
    }

    /// A header that declares more setup bytes than the file holds must be
    /// a clean rejection *before* any allocation is sized from it.
    #[test]
    fn oversized_setup_len_rejected_without_allocation() {
        let server = small_server();
        let path = tmp("oversize.bin");
        save(&server, &codec(), true, &path).unwrap();
        let mut bytes = std::fs::read(&path).unwrap();
        std::fs::remove_file(&path).unwrap();

        // setup_len lives right after magic(5)+flag(1)+widths(12)+num_items(8).
        let off = 5 + 1 + 12 + 8;
        bytes[off..off + 8].copy_from_slice(&u64::MAX.to_le_bytes());
        match load_bytes(&bytes, SimpleConfig::with_lwe_dim(256), &codec()) {
            Err(StateError::Corrupt(msg)) => assert!(msg.contains("exceeds"), "unexpected rejection: {msg}"),
            Err(other) => panic!("oversized setup_len must be rejected, got {other}"),
            Ok(_) => panic!("oversized setup_len must not load"),
        }
    }

    #[test]
    fn codec_mismatch_and_corruption_rejected() {
        let server = small_server();
        let path = tmp("reject.bin");
        save(&server, &codec(), true, &path).unwrap();

        let wrong = ValueCodec {
            key_tag_bits: 16,
            ..codec()
        };
        assert!(matches!(
            load(&path, SimpleConfig::with_lwe_dim(256), &wrong),
            Err(StateError::Corrupt(_))
        ));

        // Truncated file: cut the last 100 bytes.
        let bytes = std::fs::read(&path).unwrap();
        std::fs::write(&path, &bytes[..bytes.len() - 100]).unwrap();
        assert!(load(&path, SimpleConfig::with_lwe_dim(256), &codec()).is_err());

        // Bad magic.
        let mut bad = bytes.clone();
        bad[0] ^= 0xFF;
        std::fs::write(&path, &bad).unwrap();
        assert!(matches!(
            load(&path, SimpleConfig::with_lwe_dim(256), &codec()),
            Err(StateError::Corrupt(_))
        ));

        // Trailing garbage.
        let mut long = bytes;
        long.push(0);
        std::fs::write(&path, &long).unwrap();
        assert!(matches!(
            load(&path, SimpleConfig::with_lwe_dim(256), &codec()),
            Err(StateError::Corrupt(_))
        ));

        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn acquire_state_path_refuses_sibling_suffixed_paths() {
        for reserved in ["journal", "tmp", "lock", "audit"] {
            let path = std::env::temp_dir()
                .join(format!("risepir-state-{}-selfclobber.{reserved}", std::process::id()));
            match acquire_state_path(&path) {
                Err(StateError::Io(msg)) => {
                    assert!(msg.contains(&format!(".{reserved}")), "must name the collision: {msg}")
                }
                other => panic!("a .{reserved} --state path must be refused, got {other:?}"),
            }
            assert!(!path.exists(), "the refusal must not have created (or truncated) anything at the path");
        }
    }

    #[test]
    fn acquire_state_path_locks_out_a_second_claimant() {
        let path = std::env::temp_dir().join(format!("risepir-state-{}-locked.bin", std::process::id()));
        let lock_path = path.with_extension("lock");

        let held = acquire_state_path(&path).expect("first claim succeeds");
        // flock is per open file description, so a second `File::create` +
        // `try_lock` from this same process conflicts exactly like a
        // second process would.
        match acquire_state_path(&path) {
            Err(StateError::Io(msg)) => {
                assert!(msg.contains("another process"), "must explain the conflict: {msg}");
            }
            other => panic!("a second claim on a held path must fail, got {other:?}"),
        }

        // Dropping the guard releases the claim; a fresh claim succeeds.
        drop(held);
        let _re = acquire_state_path(&path).expect("a released path can be re-claimed");

        std::fs::remove_file(&lock_path).unwrap();
    }

    /// [`STORE_ARITY`] pinned against the real compiled store type: builds
    /// an actual `Segmented2aryCuckooKVStore` (the store [`Server`] is
    /// compiled against) and checks its own reported arity matches the
    /// hand-kept mirror [`parse_raw`]'s check compares against — so a
    /// future arity change that updates `Server`'s scheme but forgets to
    /// update `STORE_ARITY` fails here immediately, not silently.
    #[test]
    fn store_arity_matches_the_compiled_store_type() {
        let codec = codec();
        let num_buckets = 2 * 64;
        let pb = simple_max_plaintext_bits(2, num_buckets / 2, 4, 32, codec.value_bits(), SimpleParams::DEFAULT_SIGMA);
        let store = Segmented2aryCuckooKVStore::new(num_buckets, 4, 32, codec.value_bits(), pb).unwrap();
        assert_eq!(store.params().arity(), STORE_ARITY);
    }

    /// A state file whose header names `SchemeKind::Segmented3ary` — the
    /// arity this binary was compiled against *before* ADR-0034 — must be
    /// rejected, and rejected *by name*: the cells section built below is
    /// sized exactly to this (otherwise well-formed) 3-ary geometry and the
    /// trailing checksum is computed correctly over it, so if the arity
    /// check in `parse_raw` did not exist, this file would load
    /// successfully (structurally, it is not corrupt at all — it is just
    /// the previous geometry lineage). Built by hand-assembling a
    /// `SetupBundle` (mirroring `risepir_http::wire`'s own test fixtures)
    /// rather than through a real 3-ary `Server`, because this crate's
    /// `Server` alias is now pinned to `Segmented2aryScheme`.
    #[test]
    fn wrong_arity_state_file_rejected_by_name_not_by_shape() {
        use ikpir_common::backend::simple::{SimpleHint, SimpleServerParams};
        use segmented_cuckoo::SchemeKind;

        let arity = 3usize;
        let lwe_dim = 4u32;
        let reshape_row_width = 6u32;
        let params = CuckooParams {
            scheme_kind: SchemeKind::Segmented3ary,
            num_buckets: 96,
            bucket_size: 4,
            fingerprint_bits: 32,
            value_bits: 96,
            plaintext_bits: 8,
        };
        assert_eq!(params.arity(), arity, "sanity: this fixture must actually be 3-ary");

        let backend_params: Vec<SimpleServerParams> = (0..arity)
            .map(|i| SimpleServerParams {
                params: SimpleParams::new(lwe_dim, 8, SimpleParams::DEFAULT_SIGMA, [i as u8; 16]),
                n_rows: 10,
                row_width: 3,
                k: 2,
                reshape_rows: 5,
                reshape_row_width,
            })
            .collect();
        let hints: Vec<SimpleHint> = (0..arity)
            .map(|i| SimpleHint {
                data: (0..lwe_dim * reshape_row_width).map(|j| (i as u32) * 1_000 + j).collect(),
            })
            .collect();
        let setup_bytes = wire::encode_setup(&SetupBundle { params, backend_params, hints, block: 0 });

        let cells_len = params.num_buckets as usize * params.bucket_size as usize * params.cells_per_slot() as usize;
        let cells = vec![0u32; cells_len];

        // key_tag(32) + balance(64) + checksum(0) = 96, matching
        // `params.value_bits` above.
        let three_ary_codec = ValueCodec { key_tag_bits: 32, balance_bits: 64, checksum_bits: 0 };
        assert_eq!(three_ary_codec.value_bits(), params.value_bits);

        let mut bytes = Vec::new();
        bytes.extend_from_slice(MAGIC);
        bytes.push(1); // complete
        for w in [three_ary_codec.key_tag_bits, three_ary_codec.balance_bits, three_ary_codec.checksum_bits] {
            bytes.extend_from_slice(&w.to_le_bytes());
        }
        bytes.extend_from_slice(&0u64.to_le_bytes()); // num_items
        bytes.extend_from_slice(&(setup_bytes.len() as u64).to_le_bytes());
        bytes.extend_from_slice(&setup_bytes);
        bytes.extend_from_slice(&(cells.len() as u64).to_le_bytes());
        for c in &cells {
            bytes.extend_from_slice(&c.to_le_bytes());
        }
        let mut hasher = xxhash_rust::xxh3::Xxh3::new();
        hasher.update(&bytes);
        bytes.extend_from_slice(&hasher.digest().to_le_bytes());

        match load_bytes(&bytes, SimpleConfig::with_lwe_dim(256), &three_ary_codec) {
            Err(StateError::Corrupt(msg)) => {
                assert!(msg.contains("arity"), "must name the arity mismatch: {msg}");
                assert!(msg.contains("re-bootstrap"), "must name the fix: {msg}");
                // Must explicitly steer *away* from "restore from backup" —
                // the advice the checksum-mismatch error above gives, and
                // the wrong one here: this file is not corrupt.
                assert!(
                    msg.contains("do not restore from backup"),
                    "must say not to restore from backup (the file itself is intact): {msg}"
                );
                assert!(
                    !msg.contains("store reconstruction"),
                    "must be rejected by the explicit STORE_ARITY check, not upstream's from_cells \
                     fallback (which would prove the check ran too late): {msg}"
                );
            }
            Err(StateError::Io(msg)) => {
                panic!("a state file from a different arity lineage must be rejected as Corrupt, not Io: {msg}")
            }
            // `LoadedState` has no `Debug` impl (not needed anywhere else),
            // so the success case is just named directly rather than
            // formatted.
            Ok(_) => panic!("a state file from a different arity lineage must not load successfully"),
        }
    }

    // ── ADR-0042: the geometry-lineage guard ────────────────────────────

    /// Builds a structurally valid RPST2 state file at an arbitrary
    /// [`CuckooParams`] — cells sized to that geometry, whole-file checksum
    /// correct — so anything that rejects it is rejecting the *geometry*,
    /// never the shape. (`wrong_arity_state_file_rejected_by_name_not_by_shape`
    /// above hand-assembles its own because it must also fake a 3-ary
    /// `SchemeKind` this crate's `Server` alias can no longer produce.)
    fn state_bytes_at(params: CuckooParams, codec: &ValueCodec) -> Vec<u8> {
        use ikpir_common::backend::simple::{SimpleHint, SimpleServerParams};

        assert_eq!(codec.value_bits(), params.value_bits, "fixture: codec must match params");
        let arity = params.arity();
        let lwe_dim = 4u32;
        let reshape_row_width = 6u32;
        let backend_params: Vec<SimpleServerParams> = (0..arity)
            .map(|i| SimpleServerParams {
                params: SimpleParams::new(
                    lwe_dim,
                    params.plaintext_bits,
                    SimpleParams::DEFAULT_SIGMA,
                    [i as u8; 16],
                ),
                n_rows: 10,
                row_width: 3,
                k: 2,
                reshape_rows: 5,
                reshape_row_width,
            })
            .collect();
        let hints: Vec<SimpleHint> = (0..arity)
            .map(|i| SimpleHint {
                data: (0..lwe_dim * reshape_row_width).map(|j| (i as u32) * 1_000 + j).collect(),
            })
            .collect();
        let setup_bytes = wire::encode_setup(&SetupBundle { params, backend_params, hints, block: 0 });
        let cells_len =
            params.num_buckets as usize * params.bucket_size as usize * params.cells_per_slot() as usize;

        let mut bytes = Vec::new();
        bytes.extend_from_slice(MAGIC);
        bytes.push(1); // complete
        for w in [codec.key_tag_bits, codec.balance_bits, codec.checksum_bits] {
            bytes.extend_from_slice(&w.to_le_bytes());
        }
        bytes.extend_from_slice(&0u64.to_le_bytes()); // num_items
        bytes.extend_from_slice(&(setup_bytes.len() as u64).to_le_bytes());
        bytes.extend_from_slice(&setup_bytes);
        bytes.extend_from_slice(&(cells_len as u64).to_le_bytes());
        for _ in 0..cells_len {
            bytes.extend_from_slice(&0u32.to_le_bytes());
        }
        let mut hasher = xxhash_rust::xxh3::Xxh3::new();
        hasher.update(&bytes);
        bytes.extend_from_slice(&hasher.digest().to_le_bytes());
        bytes
    }

    /// [`CuckooParams`] for the geometry *this binary* builds at
    /// `num_buckets` — derived, never hand-written, so these fixtures track
    /// the selector instead of pinning a number that could go stale.
    fn deployed_params_at(num_buckets: u32, codec: &ValueCodec) -> CuckooParams {
        let geom = Geometry::for_num_buckets(
            num_buckets,
            STORE_ARITY as u32,
            crate::mainnet::BUCKET_SIZE,
            crate::mainnet::FINGERPRINT_BITS,
            codec,
            Backend::Simple,
        )
        .expect("fixture geometry must be derivable");
        CuckooParams {
            scheme_kind: segmented_cuckoo::SchemeKind::Segmented2ary,
            num_buckets: geom.num_buckets,
            bucket_size: geom.bucket_size,
            fingerprint_bits: geom.fingerprint_bits,
            value_bits: geom.value_bits,
            plaintext_bits: geom.plaintext_bits,
        }
    }

    /// Positive control. Without it the two rejection tests below would
    /// still pass if the guard rejected *everything*.
    #[test]
    fn matching_geometry_passes_the_lineage_guard() {
        let codec = codec();
        assert!(check_geometry_lineage(&deployed_params_at(128, &codec), &codec).is_ok());
    }

    /// `--partial-capacity` moves `num_buckets`, so the guard derives its
    /// expectation at the *file's own* size rather than pinning the
    /// deployment's. Pinning it would refuse every partial-mode state file.
    #[test]
    fn lineage_guard_does_not_pin_num_buckets() {
        let codec = codec();
        for num_buckets in [64u32, 128, 4096, 1 << 20] {
            assert!(
                check_geometry_lineage(&deployed_params_at(num_buckets, &codec), &codec).is_ok(),
                "num_buckets {num_buckets} is a legitimate --partial-capacity size"
            );
        }
    }

    /// A state file from a `fingerprint_bits = 64` lineage — the upgrade
    /// ADR-0042 considered and rejected — must be refused *by name*. The
    /// fixture is well-formed at its own geometry, so without the guard it
    /// would load and the server would serve the previous operating point
    /// while every constant in this binary said otherwise.
    #[test]
    fn wrong_fingerprint_bits_state_file_rejected_by_name() {
        let codec = codec();
        let geom =
            Geometry::for_num_buckets(128, STORE_ARITY as u32, crate::mainnet::BUCKET_SIZE, 64, &codec, Backend::Simple)
                .unwrap();
        assert_ne!(
            geom.fingerprint_bits,
            crate::mainnet::FINGERPRINT_BITS,
            "sanity: this fixture must actually differ from what this binary builds"
        );
        let params = CuckooParams {
            scheme_kind: segmented_cuckoo::SchemeKind::Segmented2ary,
            num_buckets: geom.num_buckets,
            bucket_size: geom.bucket_size,
            fingerprint_bits: geom.fingerprint_bits,
            value_bits: geom.value_bits,
            plaintext_bits: geom.plaintext_bits,
        };
        let bytes = state_bytes_at(params, &codec);

        match load_bytes(&bytes, SimpleConfig::with_lwe_dim(256), &codec) {
            Err(StateError::Corrupt(msg)) => {
                assert!(msg.contains("fingerprint_bits"), "must name the mismatched field: {msg}");
                assert!(msg.contains("re-bootstrap"), "must name the fix: {msg}");
                assert!(
                    msg.contains("do not restore from backup"),
                    "the file is intact, not corrupt — must not steer to a backup restore: {msg}"
                );
                assert!(
                    !msg.contains("store reconstruction"),
                    "must be rejected by the explicit guard, not by upstream's from_cells fallback \
                     (which would prove the check ran after the cells were read): {msg}"
                );
            }
            Err(StateError::Io(msg)) => panic!("must be rejected as Corrupt, not Io: {msg}"),
            Ok(_) => panic!("a state file from a different fingerprint lineage must not load"),
        }
    }

    /// The sharper case, and the reason the guard derives rather than
    /// hardcodes: nobody edits this binary, but the pinned primitive's
    /// `plaintext_bits` selector retunes, so the geometry it derives today
    /// differs from the one the file was written at. No compiler can see
    /// that drift; this guard can.
    #[test]
    fn wrong_plaintext_bits_state_file_rejected_by_name() {
        let codec = codec();
        let mut params = deployed_params_at(128, &codec);
        params.plaintext_bits += 1;
        let bytes = state_bytes_at(params, &codec);

        match load_bytes(&bytes, SimpleConfig::with_lwe_dim(256), &codec) {
            Err(StateError::Corrupt(msg)) => {
                assert!(msg.contains("plaintext_bits"), "must name the mismatched field: {msg}");
                assert!(msg.contains("re-bootstrap"), "must name the fix: {msg}");
                assert!(
                    !msg.contains("store reconstruction"),
                    "must be rejected before the cells section is read: {msg}"
                );
            }
            Err(StateError::Io(msg)) => panic!("must be rejected as Corrupt, not Io: {msg}"),
            Ok(_) => panic!("a state file from a different plaintext_bits lineage must not load"),
        }
    }
}
