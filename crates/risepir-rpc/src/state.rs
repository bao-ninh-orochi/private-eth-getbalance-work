//! Server state persistence: one file holding everything
//! [`RisePirServer::from_parts`] needs to reassemble a mainnet deployment
//! after a restart — cells, per-segment `A` params + hints, block, item
//! count, codec widths, and whether the account set is *complete*
//! (snapshot-bootstrapped) or *partial* (`docs/deploy.md`).
//!
//! # Format (all integers little-endian)
//!
//! ```text
//! magic  b"RPST2"
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
//! [`StateError::Corrupt`] at load. Legacy `RPST1` files still load
//! (with a stderr warning) so existing deployments upgrade on their next
//! save rather than re-bootstrapping.
//!
//! # Scale note
//!
//! At mainnet-complete scale the cells section is ~10 GB; save/load are
//! streamed through fixed 64 Ki-cell chunks, never materialising a second
//! byte-copy of the array.

use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::Path;

use ikpir_common::{HintPatchMode, IncrementalPirBackend, IndexPirBackend, SimplePirBackend};
use risepir_http::wire;
use risepir_proto::{BlockDelta, ValueCodec};
use risepir_server::{RisePirServer, SetupBundle};
use segmented_cuckoo::{CuckooParams, Segmented3aryCuckooKVStore, Segmented3aryScheme};

use crate::journal::{self, JournalReader, ScanStop};

const MAGIC_V1: &[u8; 5] = b"RPST1";
const MAGIC: &[u8; 5] = b"RPST2";
/// Cells per streamed chunk (256 KiB of bytes at 4 B/cell).
const CHUNK_CELLS: usize = 64 * 1024;

/// The concrete server type this deployment persists.
pub type Server = RisePirServer<Segmented3aryScheme, SimplePirBackend>;

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
    /// The file's trailing whole-file xxh3 digest — `Some` for `RPST2`
    /// (the verified checksum every current save produces), `None` for a
    /// legacy `RPST1` file (no such checksum exists to report). A delta
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
    Ok(SaveReport { bytes: total, digest })
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

/// The single decode path behind [`load`] / [`load_bytes`] / [`load_raw`].
/// `total_len` is the input's real size, used to bound every
/// header-declared length *before* it sizes an allocation — a corrupt or
/// hostile file must produce a clean [`StateError`], never an OOM (the
/// repo's validate-every-length rule applies to state files too, not just
/// the network). Stops short of store reconstruction — see [`RawState`]
/// and [`assemble`].
fn parse_raw(reader: impl Read, total_len: u64, codec: &ValueCodec) -> Result<RawState, StateError> {
    let mut r = HashingReader {
        inner: reader,
        hasher: xxhash_rust::xxh3::Xxh3::new(),
    };

    let mut magic = [0u8; 5];
    r.read_exact(&mut magic).map_err(io_err)?;
    let checksummed = match &magic {
        m if m == MAGIC => true,
        m if m == MAGIC_V1 => {
            eprintln!(
                "risepir-rpc: WARNING: legacy RPST1 state file (no whole-file checksum) — \
                 loading with structural checks only; the next save upgrades it to RPST2"
            );
            false
        }
        _ => return Err(StateError::Corrupt("bad magic (not an RPST1/RPST2 state file)".to_string())),
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

    let digest = if checksummed {
        // v2: the whole-file xxh3 covers every byte read so far. Read the
        // stored digest *unhashed* (it cannot cover itself) and compare.
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
    } else {
        None
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
/// store via `Segmented3aryCuckooKVStore::from_cells` and the server via
/// [`Server::from_parts`]. Shared by the plain load path ([`load_from`])
/// and the journal-restore path ([`load_with_journal_restore`]), which
/// mutates `cells` / `setup.hints` / `num_items` / `setup.block` in place
/// before calling this.
pub(crate) fn assemble(raw: RawState, config: ikpir_common::SimpleConfig, codec: ValueCodec) -> Result<LoadedState, StateError> {
    let store = Segmented3aryCuckooKVStore::from_cells(raw.cells, raw.setup.params, raw.num_items)
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
            Self::State(e) => write!(f, "{e}"),
            Self::ApplyFailure(m) => write!(f, "journal replay apply-time failure: {m}"),
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
        (header.base_digest == base_digest).then_some(reader)
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
        let num_buckets = 3 * 64;
        let pb = simple_max_plaintext_bits(num_buckets / 3, 4, 32, codec.value_bits(), SimpleParams::DEFAULT_SIGMA);
        let store = Segmented3aryCuckooKVStore::new(num_buckets, 4, 32, codec.value_bits(), pb).unwrap();
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

    /// A legacy RPST1 file (no trailing checksum) still loads — existing
    /// deployments upgrade on their next save instead of re-bootstrapping.
    /// Constructed from a real v2 file: strip the 8-byte trailer, patch
    /// the magic — that *is* the v1 format.
    #[test]
    fn legacy_rpst1_still_loads() {
        let mut server = small_server();
        let addr = keccak256(&[9u8; 20]);
        server
            .apply_block(&BlockUpdate { block: 3, changes: vec![(addr, 777u128)], credits: vec![] })
            .unwrap();
        let path = tmp("legacy.bin");
        save(&server, &codec(), false, &path).unwrap();

        let mut bytes = std::fs::read(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        bytes.truncate(bytes.len() - 8);
        bytes[..5].copy_from_slice(MAGIC_V1);

        let loaded = load_bytes(&bytes, SimpleConfig::with_lwe_dim(256), &codec()).unwrap();
        assert!(!loaded.complete);
        assert_eq!(loaded.server.block(), 3);
        assert_eq!(loaded.server.balance_of(&addr).unwrap(), Some(777));
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
}
