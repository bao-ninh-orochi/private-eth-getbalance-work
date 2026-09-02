//! `risepir-rpc time-setup --state <file> [--out <json-path>]` (C13): loads
//! a state file exactly the way `mainnet --state` does, times a full PIR
//! setup recompute over the loaded store, and reports the result.
//!
//! # What "freshly computed" means here, and why this is not a raw byte
//! compare against the persisted hints
//!
//! [`SimplePirBackend::server_setup`] — the function every fresh bootstrap
//! and [`RisePirServer::full_rebuild`] both call — samples a **fresh
//! random seed** for the per-segment public matrix `A` on every call ("no
//! way to inject one through the public API", pinned by
//! `risepir-server`'s own `tests/full_rebuild_alloc.rs`). So a freshly
//! recomputed hint and the persisted, incrementally-patched hint are
//! built from *different* `A` matrices even when the underlying cells are
//! byte-identical — comparing their raw ciphertext bytes would report a
//! mismatch on every healthy run, which would make `hints_match_persisted`
//! meaningless as a regression signal (always false, whether or not
//! anything is actually wrong).
//!
//! What *is* a meaningful, checkable statement of the same invariant
//! (`hints[j] == A_j · D_j`, `RisePirServer`'s own documented contract) is
//! **decoding**: build a client from a hint (any hint, any `A`) and the
//! matching `ServerParams`, query a handful of representative rows per
//! segment, and check the decoded plaintext equals the store's own raw
//! cells at that row — exactly the correctness check
//! `full_rebuild_alloc.rs` already established for verifying a
//! freshly-rebuilt hint (its own doc comment states the identical
//! reasoning: "two independently built servers over identical cells would
//! end up with different `A` and therefore different hint bytes
//! regardless of whether either is correct... querying the rebuilt server
//! through a client built from its own post-rebuild `setup()` bundle
//! sidesteps that confound entirely").
//!
//! [`compute`] therefore checks the invariant **twice**, both times by
//! decoding, never by raw-byte comparison:
//!
//! 1. Before timing anything: the **persisted** hints (loaded from the
//!    state file, incrementally patched forward by every block since
//!    bootstrap) must still decode to the store's current cells — this is
//!    the real regression test, the one that would actually catch a bug
//!    in `fold_mutations_into_row_deltas`/`server_patch_hint`'s
//!    incremental math drifting from the store over many applied blocks.
//! 2. After timing the rebuild: the **freshly recomputed** hints must
//!    also decode to the same cells — an operational sanity check that a
//!    multi-minute, memory-heavy, `rayon`-parallel setup at real
//!    deployment scale did not silently corrupt anything, which the
//!    timing measurement alone would not catch.
//!
//! `hints_match_persisted` is `true` only when both hold. `setup_seconds`
//! (C13's own number) is unaffected by any of this — wall-clock time does
//! not depend on which random seed `server_setup` happened to draw.

use std::path::Path;
use std::time::Instant;

use ikpir_common::{IndexPirBackend, SimpleConfig, SimplePirBackend};

use crate::state;

/// Everything `time-setup` measures and reports — see the module docs for
/// what `hints_match_persisted` actually checks (and why it is not a raw
/// hint-byte comparison). `Serialize` derives the exact JSON shape
/// `--out` writes: field names are the JSON keys verbatim, no renames.
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
    /// (uniform across every segment of one deployment).
    pub lwe_dim: u32,
    /// `CuckooParams::plaintext_bits`.
    pub plaintext_bits: u32,
    /// Wall-clock seconds of the setup computation alone — timed around
    /// `RisePirServer::full_rebuild()` only; the state-file load and the
    /// decode-based comparisons are both untimed.
    pub setup_seconds: f64,
    /// Whether both decode-based checks the module docs describe passed.
    /// `false` is a real defect, never a benign difference — see the
    /// module docs for exactly what this does and does not compare.
    pub hints_match_persisted: bool,
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
/// (which would cost as much as the setup itself, once per row).
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
/// hints and params, and checks each decoded row equals the store's own
/// raw cells at that `(segment, row)` — the operational form of the
/// `hints[j] == A_j · D_j` invariant (see the module docs for why this,
/// not a raw byte compare, is the meaningful check). Never mutates
/// `server`.
fn hints_decode_correctly(server: &state::Server) -> bool {
    let params = server.params();
    let segment_size = params.segment_size();
    let row_width = params.bucket_size * params.cells_per_slot();
    let seg_cells = segment_size as usize * row_width as usize;
    let cells = server.cells();

    let bundle = server.setup();
    let mut states: Vec<_> = bundle
        .backend_params
        .iter()
        .zip(&bundle.hints)
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
/// is a borrow throughout) plus at most two hint sets at once (the
/// pre-rebuild `setup()` bundle used for the first decode check, dropped
/// before the second) — never more.
pub fn compute(mut server: state::Server) -> TimeSetupReport {
    let accounts = server.num_items();
    let params = server.params();
    let state_block = server.block();
    let cells_bytes = server.cells().len() as u64 * 4;

    let persisted_ok = hints_decode_correctly(&server);

    let t0 = Instant::now();
    server.full_rebuild();
    let setup_seconds = t0.elapsed().as_secs_f64();

    let fresh_ok = hints_decode_correctly(&server);

    let hint_bytes = server.hint_bytes();
    let lwe_dim = server.setup().backend_params[0].params.lwe_dim;

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
        hints_match_persisted: persisted_ok && fresh_ok,
        state_block,
        rayon_threads: rayon::current_num_threads(),
    }
}

/// Prints one line per fact in `report` to stdout (untimestamped CLI
/// output, like this binary's other banners/usage text — `logln!` is for
/// the running server's own log, not this one-shot report).
fn print_report(report: &TimeSetupReport) {
    println!("accounts:              {}", report.accounts);
    println!("buckets:                {}", report.buckets);
    println!(
        "cells_bytes:            {} ({:.2} GB)",
        report.cells_bytes,
        report.cells_bytes as f64 / 1e9
    );
    println!(
        "hint_bytes:             {} ({:.2} MB)",
        report.hint_bytes,
        report.hint_bytes as f64 / 1e6
    );
    println!("arity:                  {}", report.arity);
    println!("bucket_size:            {}", report.bucket_size);
    println!("lwe_dim:                {}", report.lwe_dim);
    println!("plaintext_bits:         {}", report.plaintext_bits);
    println!("rayon_threads:          {}", report.rayon_threads);
    println!("state_block:            {}", report.state_block);
    println!("setup_seconds:          {:.3}", report.setup_seconds);
    println!("hints_match_persisted:  {}", report.hints_match_persisted);
}

/// Runs the full `time-setup` subcommand: loads `state_path` exactly the
/// way `mainnet --state` does (`state::load`, the `from_parts` path —
/// untimed I/O), runs [`compute`], prints one line per fact, and — when
/// `out_path` is given — writes `report` as JSON (see
/// [`TimeSetupReport`]'s field docs for the exact shape). Returns the
/// process exit code `main.rs` should use: `0` on success, non-zero if
/// the state file failed to load or the hints did not match (a real
/// defect, per [`TimeSetupReport::hints_match_persisted`]'s docs).
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

    if report.hints_match_persisted {
        0
    } else {
        eprintln!(
            "risepir-rpc time-setup: FATAL: hints_match_persisted=false — the persisted hint no \
             longer decodes to the store's own cells (or the freshly recomputed one does not); \
             this is a real defect, not a benign difference (see this module's doc comment)."
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
    /// `hints_match_persisted == true` — the persisted, incrementally
    /// patched hints (and the freshly recomputed ones) both still decode
    /// to the store's actual cells.
    #[test]
    fn compute_reports_hints_match_persisted_true_after_patched_blocks() {
        let server = small_patched_server();
        let expected_accounts = server.num_items();
        let expected_block = server.block();

        let report = compute(server);

        assert!(
            report.hints_match_persisted,
            "a correctly patched server's hints must decode-verify clean: {report:?}"
        );
        assert_eq!(report.accounts, expected_accounts);
        assert_eq!(report.state_block, expected_block);
        assert_eq!(report.arity, ARITY);
        assert_eq!(report.bucket_size, BUCKET_SIZE);
        assert_eq!(report.buckets, NUM_BUCKETS);
        assert_eq!(report.lwe_dim, LWE_DIM);
        assert!(report.setup_seconds >= 0.0);
        assert!(report.cells_bytes > 0);
        assert!(report.hint_bytes > 0);
        assert!(report.rayon_threads >= 1);
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
        assert!(hints_decode_correctly(&server), "sanity: starts correct");

        let params = server.params();
        let num_items = server.num_items();
        let bundle = server.setup();

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
            bundle.backend_params,
            bundle.hints,
            bundle.block,
        );

        assert!(
            !hints_decode_correctly(&mismatched_server),
            "a store cell changed out from under the hint must fail the decode check"
        );
    }
}
