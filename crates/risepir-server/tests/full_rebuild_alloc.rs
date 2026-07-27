//! Pins the failure mode `RisePirServer::full_rebuild` used to have: it
//! must never make a single allocation as large as the store's flat cell
//! array. Before this fix, `full_rebuild` snapshotted
//! `self.store.as_cells().to_vec()` "to avoid a borrow conflict" that
//! never actually existed (disjoint field borrows are fine) — at the
//! live deployment's 35.43 GB database that one `.to_vec()` took peak RSS
//! from ~38 GB to ~73 GB, turning `full_rebuild`'s own documented repair
//! path for a failed `apply_block` into an OOM. See
//! `crates/risepir-server/src/server.rs`'s `full_rebuild` for the fix and
//! its new `# Memory` doc section.
//!
//! # Why the allocator lives in a separate crate
//!
//! This file needs a `#[global_allocator]` to observe allocation sizes,
//! but every crate in this workspace — `risepir-server` included — sets
//! `unsafe_code = "forbid"`, enforced via a command-line lint flag on
//! every target in the package (integration tests included) that no
//! in-source `#[allow(unsafe_code)]` can downgrade (confirmed directly:
//! putting the `unsafe impl` straight in this file fails to compile with
//! "requested on the command line with `-F unsafe-code`"). A `GlobalAlloc`
//! impl is `unsafe fn` by the standard library's own trait definition, so
//! that one unavoidable `unsafe impl` is quarantined in the dev-only
//! `alloc-tracker` crate instead; this file only ever names the safe
//! `static` it produces (see that crate's `lib.rs` docs for the full
//! rationale).
//!
//! # Why a single `#[test]`
//!
//! `alloc_tracker`'s recording flag and running maximum are process-wide
//! statics. Integration-test binaries run every `#[test]` in the same
//! process (by default on separate threads), so a second test in this
//! file could race this one's recording window — rayon's own worker
//! threads allocating concurrently during `server_setup` is fine (their
//! allocations are small), but another unrelated `#[test]`'s allocations
//! interleaving with the recorded window would not be. Keeping this file
//! to exactly one `#[test]` sidesteps that without needing
//! `--test-threads=1`.

use std::mem::size_of_val;

use alloc_tracker::TrackingAllocator;

#[global_allocator]
static GLOBAL: TrackingAllocator = TrackingAllocator;

use ikpir_common::backend::simple::SimpleParams;
use ikpir_common::pir_params::simple_max_plaintext_bits;
use ikpir_common::{IndexPirBackend, SimpleConfig, SimplePirBackend};
use risepir_proto::geometry::Geometry;
use risepir_proto::{AddressHash, Balance, BlockUpdate, ValueCodec};
use risepir_server::RisePirServer;
use segmented_cuckoo::{Segmented3aryCuckooKVStore, Segmented3aryScheme};

// ── Test geometry — adapted from `risepir_server::server`'s own
// `#[cfg(test)] mod tests` (`crates/risepir-server/src/server.rs`), with
// `NUM_BUCKETS` raised and `LWE_DIM` lowered relative to that module's
// defaults so the per-segment hint/material buffers stay *comfortably*
// smaller than the full cell array (measured below, in the test itself)
// while keeping setup/rebuild cost small. `plaintext_bits` stays derived
// either way, per this crate's binding rule ─────────────────────────────
const ARITY: u32 = 3;
const NUM_BUCKETS: u32 = 3 * 4096;
const BUCKET_SIZE: u32 = 4;
const FINGERPRINT_BITS: u32 = 32;
const KEY_TAG_BITS: u32 = 32;
const BALANCE_BITS: u32 = 96;
const CHECKSUM_BITS: u32 = 16;
// Deliberately smaller than `server.rs`'s own test default (512): the
// property under test — "no single allocation reaches the size of the
// full cell array" — holds at any valid LWE dimension
// (`simple_max_plaintext_bits` doesn't even take `lwe_dim` as an
// argument, so shrinking it changes no other derived geometry), and a
// smaller `lwe_dim` both shrinks the one legitimate large allocation
// (the per-segment hint, `lwe_dim * reshape_row_width * 4` bytes, and the
// public-matrix material, `reshape_rows * lwe_dim * 4` bytes) and keeps
// `server_setup`'s `O(n_rows * lwe_dim * row_width)` cost down.
const LWE_DIM: u32 = 128;

/// `plaintext_bits` is *derived*, never hardcoded — this crate's binding
/// rule (`CLAUDE.md`) and exactly the pattern `server.rs`'s own tests use.
fn geometry() -> Geometry {
    let value_bits = KEY_TAG_BITS + BALANCE_BITS + CHECKSUM_BITS;
    let segment_rows = NUM_BUCKETS / ARITY;
    let plaintext_bits = simple_max_plaintext_bits(
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

fn value_codec() -> ValueCodec {
    ValueCodec {
        key_tag_bits: KEY_TAG_BITS,
        balance_bits: BALANCE_BITS,
        checksum_bits: CHECKSUM_BITS,
    }
}

fn config() -> SimpleConfig {
    SimpleConfig {
        lwe_dim: LWE_DIM,
        ..Default::default()
    }
}

fn empty_store(geom: &Geometry) -> Segmented3aryCuckooKVStore {
    Segmented3aryCuckooKVStore::new(
        geom.num_buckets,
        geom.bucket_size,
        geom.fingerprint_bits,
        geom.value_bits,
        geom.plaintext_bits,
    )
    .unwrap()
}

fn server(geom: &Geometry) -> RisePirServer<Segmented3aryScheme, SimplePirBackend> {
    RisePirServer::new(empty_store(geom), config(), value_codec(), 0)
}

/// Deterministic distinct address for index `i`, matching `server.rs`'s
/// own test helper.
fn addr(i: u64) -> AddressHash {
    let mut a = [0u8; 32];
    a[..8].copy_from_slice(&i.to_le_bytes());
    a
}

/// The one property + correctness test in this file (see the module docs
/// for why there is exactly one `#[test]` here).
#[test]
fn full_rebuild_never_copies_the_cell_array_and_stays_correct() {
    let geom = geometry();
    let mut s = server(&geom);

    // Populate through the real `apply_block` path (not a direct store
    // poke) across a few blocks, so this exercises the same mutation-log
    // -> fold -> patch plumbing a live server runs before a `full_rebuild`
    // would ever be needed.
    for block in 1u64..=3 {
        let changes: Vec<(AddressHash, Balance)> = (0..500u64)
            .map(|i| (addr(block * 10_000 + i), 1_000_000_000_000_000_000u128 + u128::from(block * 10_000 + i)))
            .collect();
        s.apply_block(&BlockUpdate { block, changes, credits: vec![] }).unwrap();
    }

    // The whole-database size a single allocation must never match.
    let db_bytes = size_of_val(s.cells());

    // ── The property under test ─────────────────────────────────────────
    alloc_tracker::start_recording();
    s.full_rebuild();
    alloc_tracker::stop_recording();
    let max_single_alloc = alloc_tracker::max_single_alloc();

    eprintln!("full_rebuild_alloc: db_bytes={db_bytes} max_single_alloc={max_single_alloc}");
    assert!(
        max_single_alloc < db_bytes,
        "full_rebuild made a single allocation as large as (or larger than) the cell array — \
         max_single_alloc={max_single_alloc} bytes, db_bytes={db_bytes} bytes"
    );

    // ── Correctness: full_rebuild's hints must still answer PIR queries
    // correctly, not merely avoid the big allocation. Compares the
    // client-decoded row against the store's own raw cells at the same
    // (segment, row) coordinate, computed with the same
    // `segment_size`/`row_width`/`seg_cells` arithmetic
    // `RisePirServer::answer` uses internally — see that method and
    // `full_rebuild` in `server.rs`. This is *not* a duplicate of an
    // existing test: `full_rebuild` is otherwise only ever mentioned in
    // doc comments in `server.rs` (`Self::full_rebuild`'s own docs,
    // `error.rs`'s recovery note) and never exercised by any `#[test]`
    // there.
    //
    // Deliberately not compared against a second, independently
    // constructed `RisePirServer`'s hints: `SimplePirBackend::server_setup`
    // samples a fresh random seed for `A` on every call (no way to inject
    // a fixed seed through the public API — see `batched_equals_per_mutation`
    // in `server.rs` for the same constraint, at length), so two
    // independently built servers over identical cells would end up with
    // different `A` and therefore different hint bytes regardless of
    // whether either is correct. Querying the rebuilt server through a
    // client built from its own post-rebuild `setup()` bundle sidesteps
    // that confound entirely. ───────────────────────────────────────────
    let params = s.params();
    let segment_size = params.segment_size();
    let row_width = params.bucket_size * params.cells_per_slot();
    let seg_cells = segment_size as usize * row_width as usize;
    let cells = s.cells();

    let bundle = s.setup();
    let mut states: Vec<_> = bundle
        .backend_params
        .iter()
        .zip(&bundle.hints)
        .map(|(p, h)| SimplePirBackend::client_setup(p, h))
        .collect();

    // Row 0 and a middle row of every segment: enough to catch an
    // off-by-one in the reshape/tiling math without querying the whole
    // (small, but not tiny) test database.
    for &row in &[0u32, segment_size / 2] {
        let queries: Vec<_> = states.iter_mut().map(|st| SimplePirBackend::client_query(st, row)).collect();
        let (responses, answered_at) = s.answer(&queries).expect("well-formed queries must be answered");
        assert_eq!(answered_at, 3, "full_rebuild must not change the server's block");

        for (j, (state, response)) in states.iter().zip(&responses).enumerate() {
            let decoded = SimplePirBackend::client_decode(state, response);
            let start = j * seg_cells + row as usize * row_width as usize;
            let expected = &cells[start..start + row_width as usize];
            assert_eq!(
                decoded, expected,
                "segment {j} row {row}: full_rebuild's hint must decode to the store's actual \
                 cells at this row"
            );
        }
    }

    // Sanity: the store itself is untouched by full_rebuild (it never
    // writes to `self.store`), so the ordinary verified read path must
    // still round-trip correctly after the rebuild.
    assert_eq!(s.balance_of(&addr(10_000)).unwrap(), Some(1_000_000_000_000_010_000u128));
}
