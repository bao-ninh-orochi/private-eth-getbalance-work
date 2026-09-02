//! `risepir-rpc time-setup --state <file> [--out <json-path>]` (C13): loads
//! a state file exactly the way `mainnet --state` does, times a full PIR
//! setup recompute over the loaded store, and — the load-bearing check —
//! reproduces the persisted hints EXACTLY from public data and asserts
//! byte-for-byte equality against what is actually on disk.
//!
//! # The exact check: `persisted_hints_exact_match`
//!
//! `hints[j] == A_j · D_j` is `RisePirServer`'s own documented invariant.
//! Reproducing it exactly, from outside the crate, needs three public
//! pieces `ikpir_common` already exposes (locations below are at the
//! pinned `v0.2.0-perf` tag this workspace builds against — see this
//! repo's own `Cargo.toml`):
//!
//! - `SimpleParams::seed` (`backend/simple/params.rs:55`) is a public
//!   field of the persisted `ServerParams` — the 16-byte seed `A_j` was
//!   sampled from at whatever bootstrap produced this state file,
//!   carried along in every restart and every `RisePirServer::setup()`
//!   bundle.
//! - `IndexPirBackend::expand_hint_material` (`backend/mod.rs:164`;
//!   `SimplePirBackend`'s impl, `simple/backend.rs:321-328`) is the
//!   public, *deterministic* seed→`A` expander — its own trait doc
//!   states the contract explicitly: "bit-identically deterministic in
//!   any seed/state inside `params`". Calling it on the persisted
//!   `params[j]` reproduces the *exact* `A_j` the deployment has used
//!   since bootstrap — no randomness, unlike `server_setup`.
//! - `IncrementalPirBackend::server_patch_hint` (already this crate's
//!   own per-block patch primitive) computes `H += Aᵀ·Δ` for a set of
//!   sparse row deltas. Patching a **zero** hint with the *entire
//!   store's cells, expressed as deltas from zero*, therefore computes
//!   exactly `Aᵀ·D` — a fresh reproduction of what the persisted hint
//!   should be.
//!
//! This module's `exact_hint_check` function does exactly this, per
//! segment: builds a zero-initialized hint of the persisted hint's own
//! length, expands `A_j` from the persisted seed, and patches in the
//! whole store — **chunked** (a small, fixed number of cuckoo-bucket
//! rows at a time, built, patched with `HintPatchMode::RowLevel`, then
//! dropped) so the
//! transcript never materialises the whole multi-GB database as deltas
//! at once. This is exact, not approximate: `server_patch_hint`'s
//! row-level realization is linear and its updates are associative
//! wrapping-`u32` addition (`ikpir-common`'s own doc comment on that
//! function: "splitting one reshape row across several rank-one updates
//! — or across two chunks — reaches the same hint"), so any chunking of
//! the same total delta set reaches the identical final hint — verified
//! at three shapes, including a ragged tail (`n_rows % k != 0`), before
//! this landed. `SimpleHint` derives `PartialEq`/`Eq`, so the comparison
//! against the persisted hint is a literal `Vec<u32>` equality — the
//! strongest statement this tool can make, and the one that gates its
//! exit code.
//!
//! `RowLevel`, not the per-block `EntryLevel` this crate's own
//! `apply_block_reporting` uses: the two realizations produce
//! bit-identical hints (`ikpir-common`'s own docs), and `RowLevel` is
//! the cheaper one here, where nearly every row of the store is
//! "touched" — entry-level's per-touched-*cell* cost loses to
//! row-level's per-touched-*row* cost once a row's own cells are this
//! dense.
//!
//! Cell offsets are carried on the wire as `u16` (`SegmentRowDeltas`),
//! so `exact_hint_check` asserts `row_width <= 65536` before building
//! any transcript — true of every geometry this codebase has ever run
//! (`bucket_size * cells_per_slot`, tens at most), so this should never
//! actually fire; it exists so a future geometry that violated it would
//! fail loudly here rather than silently misrepresenting an offset.
//!
//! # The two decode checks: diagnostics, not the exit gate
//!
//! [`compute`] also decode-verifies a handful of representative rows per
//! segment against the store's raw cells (the same "row 0 / middle /
//! last" sampling `risepir-server`'s `full_rebuild_alloc.rs` uses) —
//! `persisted_hints_decode_ok` (the persisted hints, before anything is
//! rebuilt) and `rebuilt_hints_decode_ok` (the freshly `full_rebuild()`-ed
//! ones, which necessarily use a *different*, freshly-random `A`, since
//! `SimplePirBackend::server_setup` samples a fresh seed on every call —
//! no way to inject one, pinned by `risepir-server`'s own
//! `tests/full_rebuild_alloc.rs` — so these can never be compared to the
//! persisted hints byte-for-byte). Both are reported but **never ANDed
//! together and never gate the exit code** — they are operational
//! sanity checks (does *some* independent path also agree the data
//! looks right), not the regression test. That role belongs to
//! `persisted_hints_exact_match` alone.
//!
//! `setup_seconds` (C13's own number, timed around `full_rebuild()`
//! only) and `exact_check_seconds` (the exact check's own separate
//! timer) are reported side by side but are not part of each other —
//! wall-clock setup time does not depend on which random seed
//! `server_setup` drew, and the exact check's cost is independent of it
//! too.

use std::path::Path;
use std::time::Instant;

use ikpir_common::backend::simple::{SimpleHint, SimpleServerParams};
use ikpir_common::{
    HintPatchMode, IncrementalPirBackend, IndexPirBackend, SimpleConfig, SimplePirBackend,
};
use risepir_proto::SegmentRowDeltas;

use crate::state;

/// Cuckoo-bucket rows per chunk while [`exact_hint_check`] builds the
/// whole-store-as-deltas-from-zero transcript — bounds the transcript's
/// own memory to a small, fixed multiple of `row_width` regardless of
/// segment size (the store itself can be tens of GB; this keeps each
/// chunk's `SegmentRowDeltas` in the low megabytes). Chunking is exact,
/// not an approximation — see the module docs.
const EXACT_CHECK_CHUNK_ROWS: u32 = 1024;

/// Everything `time-setup` measures and reports — see the module docs,
/// especially for what `persisted_hints_exact_match` checks and why it
/// (not the two decode fields) gates the exit code. `Serialize` derives
/// the exact JSON shape `--out` writes: field names are the JSON keys
/// verbatim, no renames.
#[derive(Debug, Clone, serde::Serialize)]
pub struct TimeSetupReport {
    /// Accounts held in the store (`RisePirServer::num_items`).
    pub accounts: u64,
    /// `CuckooParams::num_buckets`.
    pub buckets: u32,
    /// The store's raw cell array length in bytes.
    pub cells_bytes: u64,
    /// Sum, over every segment, of that segment's hint size in bytes
    /// (`RisePirServer::hint_bytes`).
    pub hint_bytes: u64,
    /// `CuckooParams::arity()`.
    pub arity: u32,
    /// `CuckooParams::bucket_size`.
    pub bucket_size: u32,
    /// `SimpleParams::lwe_dim`, read off segment 0's `ServerParams`
    /// (uniform across every segment of one deployment) via the cheap
    /// `RisePirServer::backend_params()` borrow — never `.setup()`,
    /// which would clone every hint just to read this one field.
    pub lwe_dim: u32,
    /// `CuckooParams::plaintext_bits`.
    pub plaintext_bits: u32,
    /// C13: wall-clock seconds of the setup computation alone — timed
    /// around `RisePirServer::full_rebuild()` only; the state-file load,
    /// the exact check, and the decode checks are all untimed relative
    /// to this field.
    pub setup_seconds: f64,
    /// Wall-clock seconds of the exact check alone — its own timer,
    /// separate from `setup_seconds` (see the module docs for why the
    /// two are independent numbers, not parts of one
    /// measurement).
    pub exact_check_seconds: f64,
    /// Whether the persisted hints are exactly reproduced — fresh
    /// `Aᵀ·D` from the persisted seed (`expand_hint_material`) and the
    /// store's current cells (`server_patch_hint`), compared
    /// byte-for-byte, segment by segment, against what is actually
    /// persisted. `false` is a real defect — the incremental patch path
    /// has drifted from the store — never a benign difference. This is
    /// the field [`run`] exits non-zero on; see the module docs for
    /// exactly what it does and does not compare.
    pub persisted_hints_exact_match: bool,
    /// Whether the *persisted* hints (as loaded, before anything is
    /// rebuilt) decode-verify against a sample of the store's raw cells
    /// — a diagnostic, never ANDed with `rebuilt_hints_decode_ok` and
    /// never part of the exit-code gate (see the module docs).
    pub persisted_hints_decode_ok: bool,
    /// Whether the *freshly rebuilt* hints (after `full_rebuild()`,
    /// which samples a new random seed — see the module docs)
    /// decode-verify against the same sample of raw cells — a
    /// diagnostic, never ANDed with `persisted_hints_decode_ok` and
    /// never part of the exit-code gate.
    pub rebuilt_hints_decode_ok: bool,
    /// The block the loaded state file was saved at (`RisePirServer::block`,
    /// unaffected by `full_rebuild`).
    pub state_block: u64,
    /// `rayon::current_num_threads()` at the moment setup ran — the
    /// parallelism the measured `setup_seconds` reflects.
    pub rayon_threads: usize,
}

/// Small sample of rows per segment to decode-verify against the store's
/// raw cells — the same "row 0 and a middle row" precedent
/// `risepir-server`'s `full_rebuild_alloc.rs` sets, extended with the
/// last row. Enough to catch an off-by-one in the reshape/tiling math or
/// a systematically wrong patch without paying for a full per-row decode
/// (which would cost as much as the setup itself, once per row). Only
/// feeds the two diagnostic decode checks — [`exact_hint_check`] covers
/// every row exactly, by construction.
fn sample_rows(segment_size: u32) -> Vec<u32> {
    let mut rows = vec![0u32];
    if segment_size > 1 {
        rows.push(segment_size / 2);
        rows.push(segment_size - 1);
    }
    rows.sort_unstable();
    rows.dedup();
    rows
}

/// Decodes [`sample_rows`] of every segment against `server`'s *current*
/// store cells, using the given `backend_params`/`hints` (which must
/// match what `server.answer` will actually compute with — i.e. either
/// the server's own persisted state, before anything is rebuilt, or a
/// bundle freshly re-read after `full_rebuild()` — never a stale mix of
/// the two). Checks each decoded row equals the store's own raw cells at
/// that `(segment, row)` — the operational, sampled form of the
/// `hints[j] == A_j · D_j` invariant (see the module docs for why this
/// is a diagnostic, not the exact check). Never mutates `server`.
fn hints_decode_correctly(
    server: &state::Server,
    backend_params: &[SimpleServerParams],
    hints: &[SimpleHint],
) -> bool {
    let params = server.params();
    let segment_size = params.segment_size();
    let row_width = params.bucket_size * params.cells_per_slot();
    let seg_cells = segment_size as usize * row_width as usize;
    let cells = server.cells();

    let mut states: Vec<_> = backend_params
        .iter()
        .zip(hints)
        .map(|(p, h)| SimplePirBackend::client_setup(p, h))
        .collect();

    for row in sample_rows(segment_size) {
        let queries: Vec<_> = states
            .iter_mut()
            .map(|st| SimplePirBackend::client_query(st, row))
            .collect();
        let Ok((responses, _answered_at)) = server.answer(&queries) else {
            return false;
        };
        for (j, (client_state, response)) in states.iter().zip(&responses).enumerate() {
            let decoded = SimplePirBackend::client_decode(client_state, response);
            let start = j * seg_cells + row as usize * row_width as usize;
            let expected = &cells[start..start + row_width as usize];
            if decoded != expected {
                return false;
            }
        }
    }
    true
}

/// The exact check — see the module docs for the full derivation. Per
/// segment `j`: expands `A_j` deterministically from the persisted
/// seed, patches a zero hint with the whole store's cells (expressed as
/// deltas from zero, chunked [`EXACT_CHECK_CHUNK_ROWS`] rows at a time),
/// and asserts the result equals `persisted_hints[j]` byte-for-byte.
/// Checks every segment (does not short-circuit on the first mismatch),
/// so a caller gets a complete picture and `exact_check_seconds` stays
/// comparable run to run. Never mutates `server`.
///
/// # Panics
///
/// If any segment's `row_width` exceeds `65536` — `SegmentRowDeltas`
/// carries cell offsets as `u16`, so this geometry could not be
/// represented on the wire either; see the module docs.
fn exact_hint_check(
    server: &state::Server,
    persisted_params: &[SimpleServerParams],
    persisted_hints: &[SimpleHint],
) -> (bool, f64) {
    let params = server.params();
    let segment_size = params.segment_size();
    let row_width = params.bucket_size * params.cells_per_slot();
    assert!(
        row_width <= 65536,
        "exact hint check: row_width {row_width} exceeds the u16 cell-offset capacity \
         (65536) that SegmentRowDeltas can represent"
    );
    let seg_cells = segment_size as usize * row_width as usize;
    let cells = server.cells();

    let t0 = Instant::now();
    let mut all_match = true;

    for (j, sp) in persisted_params.iter().enumerate() {
        let material = SimplePirBackend::expand_hint_material(sp);
        let mut h = SimpleHint {
            data: vec![0u32; persisted_hints[j].data.len()],
        };

        let seg_start = j * seg_cells;
        let mut row = 0u32;
        while row < segment_size {
            let chunk_end = (row + EXACT_CHECK_CHUNK_ROWS).min(segment_size);
            let mut chunk: SegmentRowDeltas = Vec::new();
            for r in row..chunk_end {
                let row_start = seg_start + r as usize * row_width as usize;
                let mut edits: Vec<(u16, i64)> = Vec::new();
                for off in 0..row_width {
                    let v = cells[row_start + off as usize];
                    if v != 0 {
                        edits.push((off as u16, i64::from(v)));
                    }
                }
                if !edits.is_empty() {
                    chunk.push((r, edits));
                }
            }
            if !chunk.is_empty() {
                SimplePirBackend::server_patch_hint(
                    sp,
                    &material,
                    &mut h,
                    &chunk,
                    HintPatchMode::RowLevel,
                );
            }
            row = chunk_end;
        }

        if h.data != persisted_hints[j].data {
            all_match = false;
        }
    }

    (all_match, t0.elapsed().as_secs_f64())
}

/// The core measurement, over an already-loaded server — separated from
/// [`run`]'s file I/O and CLI concerns so it is directly testable against
/// an in-memory server built the way `risepir-server`'s own tests do,
/// with no state file round-trip needed.
///
/// Takes ownership rather than `&mut` so a caller cannot accidentally
/// keep using `server` afterward as if it still reflected the *persisted*
/// hints — `full_rebuild` (called internally) replaces them in place with
/// a freshly, differently-seeded set (see the module docs).
///
/// # Memory
///
/// The store (owned by `server`, never cloned — `RisePirServer::cells`
/// is a borrow throughout) plus, at any one point: the persisted
/// `setup()` bundle (cloned once, held for both the persisted decode
/// check and the exact check's comparison target, then dropped before
/// `full_rebuild()` runs), one segment's `expand_hint_material` output
/// and zero-then-patched hint at a time during the exact check (dropped
/// before the next segment), and — after `full_rebuild()` — one more
/// freshly cloned `setup()` bundle for the rebuilt decode check. Never
/// more than a handful of hint-sized buffers alive at once.
pub fn compute(mut server: state::Server) -> TimeSetupReport {
    let accounts = server.num_items();
    let params = server.params();
    let state_block = server.block();
    let cells_bytes = server.cells().len() as u64 * 4;
    // Cheap borrow, no hint clone — see the field's own docs.
    let lwe_dim = server.backend_params()[0].params.lwe_dim;

    // Captured once, before `full_rebuild` overwrites `server`'s internal
    // backend_params/hints — reused for both the persisted decode check
    // and the exact check's comparison target, rather than cloning
    // twice, then dropped before the (memory-heavy) rebuild runs.
    let persisted_bundle = server.setup();
    let persisted_hints_decode_ok = hints_decode_correctly(
        &server,
        &persisted_bundle.backend_params,
        &persisted_bundle.hints,
    );
    let (persisted_hints_exact_match, exact_check_seconds) = exact_hint_check(
        &server,
        &persisted_bundle.backend_params,
        &persisted_bundle.hints,
    );
    drop(persisted_bundle);

    let t0 = Instant::now();
    server.full_rebuild();
    let setup_seconds = t0.elapsed().as_secs_f64();

    let rebuilt_bundle = server.setup();
    let rebuilt_hints_decode_ok = hints_decode_correctly(
        &server,
        &rebuilt_bundle.backend_params,
        &rebuilt_bundle.hints,
    );

    let hint_bytes = server.hint_bytes();

    TimeSetupReport {
        accounts,
        buckets: params.num_buckets,
        cells_bytes,
        hint_bytes,
        arity: params.arity() as u32,
        bucket_size: params.bucket_size,
        lwe_dim,
        plaintext_bits: params.plaintext_bits,
        setup_seconds,
        exact_check_seconds,
        persisted_hints_exact_match,
        persisted_hints_decode_ok,
        rebuilt_hints_decode_ok,
        state_block,
        rayon_threads: rayon::current_num_threads(),
    }
}

/// Prints one line per fact in `report` to stdout (untimestamped CLI
/// output, like this binary's other banners/usage text — `logln!` is for
/// the running server's own log, not this one-shot report).
fn print_report(report: &TimeSetupReport) {
    println!("accounts:                    {}", report.accounts);
    println!("buckets:                     {}", report.buckets);
    println!(
        "cells_bytes:                 {} ({:.2} GB)",
        report.cells_bytes,
        report.cells_bytes as f64 / 1e9
    );
    println!(
        "hint_bytes:                  {} ({:.2} MB)",
        report.hint_bytes,
        report.hint_bytes as f64 / 1e6
    );
    println!("arity:                       {}", report.arity);
    println!("bucket_size:                 {}", report.bucket_size);
    println!("lwe_dim:                     {}", report.lwe_dim);
    println!("plaintext_bits:              {}", report.plaintext_bits);
    println!("rayon_threads:               {}", report.rayon_threads);
    println!("state_block:                 {}", report.state_block);
    println!("setup_seconds:               {:.3}", report.setup_seconds);
    println!(
        "exact_check_seconds:         {:.3}",
        report.exact_check_seconds
    );
    println!(
        "persisted_hints_exact_match: {}",
        report.persisted_hints_exact_match
    );
    println!(
        "persisted_hints_decode_ok:   {}",
        report.persisted_hints_decode_ok
    );
    println!(
        "rebuilt_hints_decode_ok:     {}",
        report.rebuilt_hints_decode_ok
    );
}

/// Runs the full `time-setup` subcommand: loads `state_path` exactly the
/// way `mainnet --state` does (`state::load`, the `from_parts` path —
/// untimed I/O), runs [`compute`], prints one line per fact, and — when
/// `out_path` is given — writes `report` as JSON (see
/// [`TimeSetupReport`]'s field docs for the exact shape). Returns the
/// process exit code `main.rs` should use: `0` on success, non-zero if
/// the state file failed to load or `persisted_hints_exact_match` is
/// `false` (a real defect, per that field's own docs — the two decode
/// fields are diagnostics and never affect this).
pub fn run(state_path: &Path, out_path: Option<&Path>) -> i32 {
    let codec = crate::mainnet::value_codec();
    let loaded = match state::load(state_path, SimpleConfig::default(), &codec) {
        Ok(l) => l,
        Err(e) => {
            eprintln!(
                "risepir-rpc time-setup: fatal: loading {}: {e}",
                state_path.display()
            );
            return 1;
        }
    };

    let report = compute(loaded.server);
    print_report(&report);

    if let Some(out_path) = out_path {
        let json = match serde_json::to_string_pretty(&report) {
            Ok(j) => j,
            Err(e) => {
                eprintln!("risepir-rpc time-setup: fatal: encoding --out JSON: {e}");
                return 1;
            }
        };
        if let Err(e) = std::fs::write(out_path, json) {
            eprintln!(
                "risepir-rpc time-setup: fatal: writing {}: {e}",
                out_path.display()
            );
            return 1;
        }
        println!("wrote {}", out_path.display());
    }

    if report.persisted_hints_exact_match {
        0
    } else {
        eprintln!(
            "risepir-rpc time-setup: FATAL: persisted_hints_exact_match=false — the persisted, \
             incrementally-patched hint no longer equals a fresh, exact Aᵀ·D reproduction built \
             from the same seed and the store's current cells; this is a real defect, not a \
             benign difference (see this module's doc comment for exactly what is compared)."
        );
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ikpir_common::backend::simple::SimpleParams;
    use ikpir_common::pir_params::simple_max_plaintext_bits;
    use risepir_proto::geometry::Geometry;
    use risepir_proto::{AddressHash, Balance, BlockUpdate};
    use risepir_server::RisePirServer;
    use segmented_cuckoo::Segmented2aryCuckooKVStore;

    const ARITY: u32 = 2;
    const NUM_BUCKETS: u32 = 2 * 1024;
    const BUCKET_SIZE: u32 = 4;
    const FINGERPRINT_BITS: u32 = 32;
    const KEY_TAG_BITS: u32 = 32;
    const BALANCE_BITS: u32 = 96;
    const CHECKSUM_BITS: u32 = 16;
    const LWE_DIM: u32 = 128;

    fn geometry() -> Geometry {
        let value_bits = KEY_TAG_BITS + BALANCE_BITS + CHECKSUM_BITS;
        let segment_rows = NUM_BUCKETS / ARITY;
        let plaintext_bits = simple_max_plaintext_bits(
            ARITY,
            segment_rows,
            BUCKET_SIZE,
            FINGERPRINT_BITS,
            value_bits,
            SimpleParams::DEFAULT_SIGMA,
        );
        Geometry {
            arity: ARITY,
            num_buckets: NUM_BUCKETS,
            bucket_size: BUCKET_SIZE,
            fingerprint_bits: FINGERPRINT_BITS,
            value_bits,
            plaintext_bits,
        }
    }

    fn addr(i: u64) -> AddressHash {
        let mut a = [0u8; 32];
        a[..8].copy_from_slice(&i.to_le_bytes());
        a
    }

    fn small_patched_server() -> state::Server {
        let geom = geometry();
        let store = Segmented2aryCuckooKVStore::new(
            geom.num_buckets,
            geom.bucket_size,
            geom.fingerprint_bits,
            geom.value_bits,
            geom.plaintext_bits,
        )
        .unwrap();
        let mut server = RisePirServer::new(
            store,
            SimpleConfig {
                lwe_dim: LWE_DIM,
                ..Default::default()
            },
            crate::mainnet::value_codec(),
            0,
        );

        // Patch forward across several blocks through the real
        // apply_block path (mutation log -> fold -> EntryLevel patch),
        // exactly the incremental history a live deployment's persisted
        // state file would have accumulated.
        for block in 1u64..=5 {
            let changes: Vec<(AddressHash, Balance)> = (0..200u64)
                .map(|i| {
                    (
                        addr(block * 1_000 + i),
                        1_000_000_000_000_000_000u128 + u128::from(block * 1_000 + i),
                    )
                })
                .collect();
            server
                .apply_block(&BlockUpdate {
                    block,
                    changes,
                    credits: vec![],
                })
                .unwrap();
        }
        server
    }

    /// The regression test item 7 asks for: `time-setup`'s `compute` on a
    /// small mock server after N patched blocks reports
    /// `persisted_hints_exact_match == true` (the exact `Aᵀ·D`
    /// reproduction from the persisted seed matches the persisted,
    /// incrementally-patched hint byte-for-byte), and both decode
    /// diagnostics also agree.
    #[test]
    fn compute_reports_the_exact_check_passing_and_both_decode_checks_ok_after_patched_blocks() {
        let server = small_patched_server();
        let expected_accounts = server.num_items();
        let expected_block = server.block();

        let report = compute(server);

        assert!(
            report.persisted_hints_exact_match,
            "a correctly patched server's hints must reproduce exactly: {report:?}"
        );
        assert!(
            report.persisted_hints_decode_ok,
            "persisted hints must also decode-verify: {report:?}"
        );
        assert!(
            report.rebuilt_hints_decode_ok,
            "freshly rebuilt hints must also decode-verify: {report:?}"
        );
        assert_eq!(report.accounts, expected_accounts);
        assert_eq!(report.state_block, expected_block);
        assert_eq!(report.arity, ARITY);
        assert_eq!(report.bucket_size, BUCKET_SIZE);
        assert_eq!(report.buckets, NUM_BUCKETS);
        assert_eq!(report.lwe_dim, LWE_DIM);
        assert!(report.setup_seconds >= 0.0);
        assert!(report.exact_check_seconds >= 0.0);
        assert!(report.cells_bytes > 0);
        assert!(report.hint_bytes > 0);
        assert!(report.rayon_threads >= 1);
    }

    /// Direct pin of the exact check itself (not only through
    /// `compute`): on a small server patched across several blocks,
    /// reproducing `Aᵀ·D` from the persisted seed and the store's
    /// current cells must equal the persisted, incrementally-patched
    /// hint byte-for-byte, segment by segment.
    #[test]
    fn exact_hint_check_passes_on_a_correctly_patched_server() {
        let server = small_patched_server();
        let bundle = server.setup();
        let (matched, seconds) = exact_hint_check(&server, &bundle.backend_params, &bundle.hints);
        assert!(matched, "exact check must pass on correctly patched hints");
        assert!(seconds >= 0.0);
    }

    /// The exact check must actually be sensitive to a real mismatch: a
    /// single flipped hint word must fail it. Corrupts a copy of the
    /// persisted hint directly — simpler than desynchronizing the store,
    /// and this function's whole point is comparing against exactly the
    /// hint bytes it is handed, so corrupting that argument directly
    /// exercises the same comparison a real drift would fail.
    #[test]
    fn exact_hint_check_fails_when_a_hint_word_is_flipped() {
        let server = small_patched_server();
        let bundle = server.setup();
        let (matched, _) = exact_hint_check(&server, &bundle.backend_params, &bundle.hints);
        assert!(matched, "sanity: starts correct");

        let mut corrupted_hints = bundle.hints.clone();
        corrupted_hints[0].data[0] ^= 1;

        let (matched, _) = exact_hint_check(&server, &bundle.backend_params, &corrupted_hints);
        assert!(
            !matched,
            "a single flipped hint word must fail the exact check"
        );
    }

    /// `hints_decode_correctly` must actually be sensitive to a real
    /// mismatch, not vacuously `true`. `RisePirServer` exposes no way to
    /// mutate a live store's cells directly (by design — every write goes
    /// through `apply_block`, which keeps hint and store in sync), so this
    /// builds the mismatch the only way available from outside the crate:
    /// reassemble a server via `RisePirServer::from_parts` from a
    /// *correct* server's hints paired with a cell array that has one
    /// cell flipped after the hint was computed — a hint now stale
    /// relative to the store it is handed with, exactly the drift a real
    /// `fold`/`server_patch_hint` bug would produce. Exercises the
    /// detection path item 7 asks this test to cover ("that would be a
    /// real defect").
    #[test]
    fn hints_decode_correctly_detects_a_real_mismatch() {
        let server = small_patched_server();
        let bundle = server.setup();
        assert!(
            hints_decode_correctly(&server, &bundle.backend_params, &bundle.hints),
            "sanity: starts correct"
        );

        let params = server.params();
        let num_items = server.num_items();

        let mut cells = server.snapshot_cells();
        // Flip the low bit of the very first cell (segment 0, cuckoo
        // bucket 0, offset 0 — inside reshape row 0, which `sample_rows`
        // always queries) — the hint above was computed *before* this
        // change, so it now describes stale data at that cell.
        cells[0] ^= 1;
        let mismatched_store =
            Segmented2aryCuckooKVStore::from_cells(cells, params, num_items).unwrap();
        let mismatched_server = RisePirServer::from_parts(
            mismatched_store,
            SimpleConfig {
                lwe_dim: LWE_DIM,
                ..Default::default()
            },
            crate::mainnet::value_codec(),
            bundle.backend_params.clone(),
            bundle.hints.clone(),
            bundle.block,
        );

        assert!(
            !hints_decode_correctly(&mismatched_server, &bundle.backend_params, &bundle.hints),
            "a store cell changed out from under the hint must fail the decode check"
        );
    }
}
