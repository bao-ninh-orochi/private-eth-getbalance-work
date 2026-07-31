//! End-to-end wiring + correctness: `MockFeed` -> `RisePirServer::apply_block`
//! -> `DeltaRing` -> `RisePirClient`, with the client's private answers diffed
//! against the mock's exact ground truth across every account category
//! (`docs/plan.md` §3, Stage 0.2; the full Stage-0.5 conformance harness
//! scales this up to the ≥1000 × ≥100 gate).
//!
//! The load-bearing assertion is that a **deleted** account reads back as
//! exactly `Lookup::NotFound` — not merely "balance 0". A real delete empties
//! the slot, so the client's fp∧key_tag scan finds nothing → `NotFound`. An
//! `apply_block` bug that stored a zero *value* instead of deleting would
//! leave `fp ‖ key_tag ‖ encode(0)` in the slot → `Found(0)`; since
//! `balance_of(deleted) == 0`, only checking `NotFound` distinguishes the two.
//! This is what pins delete-on-zero (ADR-0015) end-to-end.

use std::collections::{HashMap, HashSet};

use ikpir_common::{SimpleConfig, SimplePirBackend};
use risepir_client::{Lookup, RisePirClient};
use risepir_feed::{Feed, MockConfig, MockFeed};
use risepir_proto::{AddressHash, Backend, Balance, Geometry, ValueCodec};
use risepir_server::{DeltaRing, RisePirServer};
use segmented_cuckoo::{Segmented2aryCuckooKVStore, Segmented2aryScheme};

const ARITY: u32 = 2;
const BUCKET_SIZE: u32 = 4;
const FINGERPRINT_BITS: u32 = 32;
const LWE_DIM: u32 = 512;
const BLOCKS: u64 = 30;

fn codec() -> ValueCodec {
    ValueCodec {
        key_tag_bits: 32,
        balance_bits: 96,
        checksum_bits: 16,
    }
}

/// One private lookup, end to end: build the query, answer it at the server's
/// head, rewind + scan on the client.
fn private_lookup(
    client: &mut RisePirClient<SimplePirBackend>,
    server: &RisePirServer<Segmented2aryScheme, SimplePirBackend>,
    addr: &AddressHash,
) -> Lookup {
    let (queries, ctx) = client.build_query(addr);
    let (resp, at_block) = server.answer(&queries).expect("answer");
    client.finish(addr, &ctx, resp, at_block).expect("finish")
}

#[test]
fn mock_pipeline_matches_ground_truth_across_all_categories() {
    let cfg = MockConfig {
        seed: 0x0EDE_FACE_1234_5678,
        num_genesis_keys: 3_000,
        changes_per_block: 40,
        inserts_per_block: 10,
        deletes_per_block: 10, // == inserts ⇒ live-set size invariant ⇒ no TableFull
    };
    let mut feed = MockFeed::new(cfg.clone());
    let codec = codec();

    // Size the geometry from the genesis account count (75% target load; the
    // invariant live set keeps us there), then build the store from genesis.
    let geom = Geometry::for_accounts(
        cfg.num_genesis_keys,
        ARITY,
        BUCKET_SIZE,
        FINGERPRINT_BITS,
        &codec,
        Backend::Simple,
    )
    .expect("geometry");
    let mut store = Segmented2aryCuckooKVStore::new(
        geom.num_buckets,
        geom.bucket_size,
        geom.fingerprint_bits,
        geom.value_bits,
        geom.plaintext_bits,
    )
    .expect("store");
    for (addr, bal) in feed.snapshot() {
        let v = codec.encode(&addr, bal).expect("encode genesis");
        store.insert(addr, &v).expect("insert genesis");
    }

    let mut server: RisePirServer<Segmented2aryScheme, SimplePirBackend> =
        RisePirServer::new(store, SimpleConfig::with_lwe_dim(LWE_DIM), codec, 0);
    let mut ring = DeltaRing::new(300);
    let mut client: RisePirClient<SimplePirBackend> =
        RisePirClient::from_setup(server.setup(), codec);

    // Follow the mock, wiring every block through apply_block, the ring, and
    // the client's rolling delta. Meanwhile collect category samples.
    let mut deleted: Vec<AddressHash> = Vec::new();
    let mut activity: HashMap<AddressHash, u32> = HashMap::new();
    for _ in 0..BLOCKS {
        let upd = feed
            .next_block()
            .expect("feed")
            .expect("mock always has a next block");
        for (addr, bal) in &upd.changes {
            if *bal == 0 {
                deleted.push(*addr);
            } else {
                *activity.entry(*addr).or_insert(0) += 1;
            }
        }
        let delta = server.apply_block(&upd).expect("apply_block");
        ring.push(delta.clone());
        client.ingest_delta(&delta).expect("ingest_delta");
    }

    assert_eq!(server.block(), BLOCKS);
    assert_eq!(ring.head(), Some(BLOCKS));
    assert_eq!(
        client.pinned_block(),
        0,
        "hint stays pinned at genesis; no GC"
    );

    // ── Build the category samples ──────────────────────────────────────
    let live_set: HashSet<AddressHash> = feed.live_keys().iter().copied().collect();

    // DELETED: emitted with balance 0 and still gone (deletes are never
    // re-inserted — inserts use fresh ids). Ground truth is 0.
    let deleted_sample: Vec<AddressHash> = deleted
        .iter()
        .copied()
        .filter(|a| feed.balance_of(a) == 0 && !live_set.contains(a))
        .take(8)
        .collect();
    assert!(
        !deleted_sample.is_empty(),
        "sample must contain deleted keys (non-vacuous)"
    );

    // HIGH-ACTIVITY: still-live keys touched the most times during the run.
    let mut activity_live: Vec<(AddressHash, u32)> = activity
        .iter()
        .filter(|(a, _)| live_set.contains(*a))
        .map(|(a, c)| (*a, *c))
        .collect();
    activity_live.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let high_activity: Vec<AddressHash> = activity_live.iter().take(6).map(|(a, _)| *a).collect();
    assert!(
        !high_activity.is_empty(),
        "sample must contain touched (high-activity) keys"
    );

    // LIVE-UNTOUCHED: genesis keys still live that were never in the change
    // stream (a low block count leaves most genesis keys untouched).
    let untouched: Vec<AddressHash> = feed
        .live_keys()
        .iter()
        .copied()
        .filter(|a| !activity.contains_key(a))
        .take(8)
        .collect();
    assert!(
        !untouched.is_empty(),
        "sample must contain untouched-since-genesis keys"
    );

    // NEVER-EXISTED: ids the mock never generates (its ids stay well below 1e7).
    let never: Vec<AddressHash> = (0..8u64)
        .map(|i| MockFeed::address_for(10_000_000 + i))
        .collect();
    for a in &never {
        assert_eq!(
            feed.balance_of(a),
            0,
            "sanity: never-existed keys are absent from ground truth"
        );
    }

    // ── Diff every sampled private answer against ground truth ──────────
    // DELETED must be NotFound *specifically* (see the module docs).
    for addr in &deleted_sample {
        assert_eq!(
            private_lookup(&mut client, &server, addr),
            Lookup::NotFound,
            "a deleted account must read back as NotFound (a real removal, not a stored zero)"
        );
    }

    // NEVER-EXISTED must be NotFound too.
    for addr in &never {
        assert_eq!(
            private_lookup(&mut client, &server, addr),
            Lookup::NotFound,
            "a never-existed account must read back as NotFound"
        );
    }

    // LIVE (high-activity + untouched) must resolve to the exact ground-truth balance.
    for addr in high_activity.iter().chain(untouched.iter()) {
        let expected: Balance = feed.balance_of(addr);
        assert_ne!(
            expected, 0,
            "sanity: a live key has a nonzero ground-truth balance"
        );
        match private_lookup(&mut client, &server, addr) {
            Lookup::Found(b) => assert_eq!(
                b, expected,
                "private balance must match ground truth for {addr:?}"
            ),
            other => panic!("live key {addr:?} resolved to {other:?}, expected Found({expected})"),
        }
    }
}
