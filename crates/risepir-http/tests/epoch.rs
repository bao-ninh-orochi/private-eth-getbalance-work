//! Lineage-epoch tests (ADR-0033).
//!
//! The hazard these pin down: `segmented_cuckoo` lays out its table
//! nondeterministically (slot and eviction choices come from
//! `rand::rng()`), and `server_setup` samples fresh LWE seeds, so two
//! bootstraps over the *same* account universe produce hints and deltas
//! that are mutually meaningless. Before ADR-0033, a client pinned at
//! block `P` of bootstrap 1 could get a `200` from bootstrap 2's `/sync`
//! whenever bootstrap 2's replay head passed within ring range of `P` —
//! and mismatched-lineage deltas applied to that client's hint make the
//! fingerprint scan miss, which complete mode maps to `0x0`: a silently
//! wrong balance. The epoch gate turns every such request into a `409`.

use std::sync::Arc;

use axum::body::{to_bytes, Body};
use axum::http::{header, Request, StatusCode};
use tower::ServiceExt;

use ikpir_common::{SimpleConfig, SimplePirBackend};
use risepir_feed::{Feed, MockConfig, MockFeed};
use risepir_http::{wire, NodeState};
use risepir_proto::{Backend, Geometry, ValueCodec};
use risepir_server::{DeltaRing, RisePirServer};
use segmented_cuckoo::{Segmented2aryCuckooKVStore, Segmented2aryScheme};

const ARITY: u32 = 2;
const BUCKET_SIZE: u32 = 4;
const FINGERPRINT_BITS: u32 = 32;
const LWE_DIM: u32 = 512;

fn codec() -> ValueCodec {
    ValueCodec {
        key_tag_bits: 32,
        balance_bits: 96,
        checksum_bits: 16,
    }
}

/// One bootstrap over the fixed mock universe. Called twice, this is two
/// *lineages* of the same deployment — same accounts, same balances,
/// independently sampled seeds and layout, which is exactly what a
/// crash-and-re-bootstrap produces in production.
fn bootstrap_node() -> (Arc<NodeState>, MockFeed) {
    let cfg = MockConfig {
        seed: 0xD1CE_D1CE_D1CE_D1CE,
        num_genesis_keys: 1_000,
        changes_per_block: 20,
        inserts_per_block: 5,
        deletes_per_block: 5,
    };
    let feed = MockFeed::new(cfg.clone());
    let value_codec = codec();
    let geom = Geometry::for_accounts(
        cfg.num_genesis_keys,
        ARITY,
        BUCKET_SIZE,
        FINGERPRINT_BITS,
        &value_codec,
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
        let v = value_codec.encode(&addr, bal).expect("encode genesis");
        store.insert(addr, &v).expect("insert genesis");
    }
    let server: RisePirServer<Segmented2aryScheme, SimplePirBackend> =
        RisePirServer::new(store, SimpleConfig::with_lwe_dim(LWE_DIM), value_codec, 0);
    let state = Arc::new(NodeState::new(server, DeltaRing::new(300), true));
    (state, feed)
}

async fn get(app: &axum::Router, uri: &str) -> (StatusCode, Vec<u8>) {
    let resp = app
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let body = to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap()
        .to_vec();
    (status, body)
}

/// Two bootstraps over the identical universe are distinct lineages: their
/// epochs must differ (they derive from independently sampled LWE seeds),
/// and each server's epoch must equal what a client derives from that
/// server's own bundle — the two sides compute it with the same function
/// from the same bytes, so agreement is by construction, but this is the
/// executable statement of that construction.
#[tokio::test]
async fn same_universe_twice_is_two_epochs_and_clients_derive_them() {
    let (a, _) = bootstrap_node();
    let (b, _) = bootstrap_node();

    assert_eq!(a.epoch().len(), 16, "epoch is 16 lowercase-hex chars");
    assert!(a
        .epoch()
        .chars()
        .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    assert_ne!(
        a.epoch(),
        b.epoch(),
        "independently sampled seeds ⇒ distinct lineage tokens"
    );

    for state in [&a, &b] {
        let app = NodeState::router(state.clone());
        let (status, body) = get(&app, "/setup").await;
        assert_eq!(status, StatusCode::OK);
        let bundle = wire::decode_setup(&body).expect("decode_setup");
        assert_eq!(
            wire::lineage_epoch(&bundle.backend_params),
            state.epoch(),
            "the client-side derivation from the served bundle must equal the server's own epoch"
        );
    }
}

/// THE regression this file exists for: a range the ring genuinely covers,
/// requested with the *other* bootstrap's epoch, must be refused — before
/// ADR-0033 this exact request returned `200` with cross-lineage deltas.
#[tokio::test]
async fn ring_covered_sync_with_other_lineages_epoch_is_refused() {
    let (a, _) = bootstrap_node();
    let (b, mut feed_b) = bootstrap_node();

    // Advance B so its ring covers (0, 10] — the window in which a
    // bootstrap-1 client pinned at 0 would, pre-ADR-0033, have been fed
    // bootstrap-2 deltas.
    for _ in 0..10 {
        let upd = feed_b
            .next_block()
            .expect("feed")
            .expect("mock always has a next block");
        b.apply_block(&upd).await.expect("apply_block");
    }
    let app_b = NodeState::router(b.clone());

    // Sanity: with B's own epoch the range is served.
    let (status, _) = get(&app_b, &format!("/sync?from=0&to=10&epoch={}", b.epoch())).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "sanity: the range itself is inside B's ring"
    );

    // The cross-lineage request: A's epoch against B's ring. 409, never a
    // delta.
    let (status, body) = get(&app_b, &format!("/sync?from=0&to=10&epoch={}", a.epoch())).await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "a ring-covered range must still be refused across lineages"
    );
    let text = String::from_utf8_lossy(&body);
    assert!(
        text.contains("epoch mismatch"),
        "the refusal must say why: {text}"
    );

    // And with no epoch at all — a pre-ADR-0033 client — the same refusal
    // (with its own message), not a silent grandfathering-in.
    let (status, body) = get(&app_b, "/sync?from=0&to=10").await;
    assert_eq!(status, StatusCode::CONFLICT);
    let text = String::from_utf8_lossy(&body);
    assert!(
        text.contains("missing epoch"),
        "absence is refused with its own message: {text}"
    );
}

/// `/delta/{block}` is `Cache-Control: immutable`, so its refusal is a
/// permanent `404` keyed by URL — an old-lineage URL must never come back
/// to life, or an HTTP cache holding it forever becomes the wrong-answer
/// channel again.
#[tokio::test]
async fn delta_by_block_is_keyed_by_epoch() {
    let (a, mut feed) = bootstrap_node();
    for _ in 0..3 {
        let upd = feed
            .next_block()
            .expect("feed")
            .expect("mock always has a next block");
        a.apply_block(&upd).await.expect("apply_block");
    }
    let app = NodeState::router(a.clone());

    let (status, _) = get(&app, &format!("/delta/2?epoch={}", a.epoch())).await;
    assert_eq!(status, StatusCode::OK);

    let (status, _) = get(&app, "/delta/2?epoch=0000000000000000").await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "another lineage's URL is permanently absent"
    );
    let (status, _) = get(&app, "/delta/2").await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "an epoch-less URL is permanently absent"
    );
}

/// `GET /setup` serves the epoch and mode as headers (the atomic
/// mode+bundle pair, ADR-0033), on the `200` and on the `304` alike; and
/// `GET /head` carries the epoch for cheap lineage-change detection.
#[tokio::test]
async fn setup_and_head_carry_epoch_and_mode_headers() {
    let (a, _) = bootstrap_node();
    let app = NodeState::router(a.clone());

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/setup")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let etag = resp
        .headers()
        .get(header::ETAG)
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    assert_eq!(
        etag,
        format!("\"setup-{}-0\"", a.epoch()),
        "the validator names the lineage"
    );
    assert_eq!(resp.headers().get("x-risepir-epoch").unwrap(), a.epoch());
    assert_eq!(
        resp.headers().get("x-risepir-mode").unwrap(),
        "1",
        "this mock deployment is complete"
    );

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/setup")
                .header(header::IF_NONE_MATCH, &etag)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_MODIFIED);
    assert_eq!(
        resp.headers().get("x-risepir-epoch").unwrap(),
        a.epoch(),
        "the 304 repeats the headers"
    );
    assert_eq!(resp.headers().get("x-risepir-mode").unwrap(), "1");

    let resp = app
        .clone()
        .oneshot(Request::builder().uri("/head").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.headers().get("x-risepir-epoch").unwrap(), a.epoch());
}

/// A block-only validator — exactly what a pre-ADR-0033 client (or a
/// cache) would present after a re-bootstrap that happened to land on the
/// same block number — must NOT revalidate: `200` with the full current
/// bundle, never a `304` blessing another lineage's bytes.
#[tokio::test]
async fn block_only_validator_never_revalidates() {
    let (a, _) = bootstrap_node();
    let app = NodeState::router(a.clone());

    // Prime the cache so the 304 path is genuinely reachable.
    let (status, _) = get(&app, "/setup").await;
    assert_eq!(status, StatusCode::OK);

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/setup")
                .header(header::IF_NONE_MATCH, "\"setup-0\"") // the pre-ADR-0033 ETag format, same block
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "a validator without the lineage must be treated as not matching"
    );
}
