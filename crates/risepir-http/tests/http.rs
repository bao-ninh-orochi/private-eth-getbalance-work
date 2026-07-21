//! In-process HTTP endpoint tests (`tower::ServiceExt::oneshot` — no real
//! network) for the RisePIR HTTP transport (`docs/plan.md` §3.4, Stage 0.3).
//!
//! (a) endpoint round-trips: `/setup`, `/head`, `POST /answer`.
//! (b) a malformed `POST /answer` body must return `400`, never panic.
//! (c) end to end: drive `NodeState::apply_block` for a MockFeed run, sync
//!     a real [`RisePirClient`] over HTTP, and diff its private answers
//!     against the mock's exact ground truth across live / deleted /
//!     never-existed addresses — mirroring `risepir-feed`'s own
//!     `mock_pipeline_matches_ground_truth_across_all_categories`, but with
//!     every PIR call now round-tripped through this crate's wire codec and
//!     axum handlers instead of calling `RisePirServer` in-process directly.

use std::sync::Arc;

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use ikpir_common::{SimpleConfig, SimplePirBackend};
use risepir_client::{Lookup, RisePirClient};
use risepir_feed::{Feed, MockConfig, MockFeed};
use risepir_http::{wire, NodeState};
use risepir_proto::{AddressHash, Backend, Geometry, ValueCodec};
use risepir_server::{DeltaRing, RisePirServer};
use segmented_cuckoo::{Segmented3aryCuckooKVStore, Segmented3aryScheme};

const ARITY: u32 = 3;
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

/// Builds a small `NodeState` from a `MockFeed` genesis population, per
/// this crate's test spec: ~2000 genesis keys, `lwe_dim` 512, arity 3,
/// `bucket_size` 4, `ValueCodec{32,96,16}`, geometry via
/// `Geometry::for_accounts`. Returns the state (wrapped for the router) and
/// the still-live `MockFeed` so callers can keep driving blocks and querying
/// ground truth.
fn build_node() -> (Arc<NodeState>, MockFeed) {
    let cfg = MockConfig {
        seed: 0xA11C_E999_0BAD_F00D,
        num_genesis_keys: 2_000,
        changes_per_block: 40,
        inserts_per_block: 10,
        deletes_per_block: 10, // == inserts ⇒ live-set size invariant ⇒ no TableFull
    };
    let feed = MockFeed::new(cfg.clone());
    let value_codec = codec();

    let geom = Geometry::for_accounts(cfg.num_genesis_keys, ARITY, BUCKET_SIZE, FINGERPRINT_BITS, &value_codec, Backend::Simple)
        .expect("geometry");
    let mut store = Segmented3aryCuckooKVStore::new(
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

    let server: RisePirServer<Segmented3aryScheme, SimplePirBackend> =
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
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap().to_vec();
    (status, body)
}

async fn post_answer(app: &axum::Router, body: Vec<u8>) -> (StatusCode, Vec<u8>) {
    let resp = app
        .clone()
        .oneshot(Request::builder().method("POST").uri("/answer").body(Body::from(body)).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap().to_vec();
    (status, body)
}

// ── (a) endpoint round-trips ────────────────────────────────────────────

#[tokio::test]
async fn setup_head_and_answer_round_trip() {
    let (state, _feed) = build_node();
    let app = NodeState::router(state);

    // GET /setup -> decode_setup
    let (status, body) = get(&app, "/setup").await;
    assert_eq!(status, StatusCode::OK);
    let bundle = wire::decode_setup(&body).expect("decode_setup");
    assert_eq!(bundle.params.arity(), ARITY as usize);
    assert_eq!(bundle.backend_params.len(), ARITY as usize);
    assert_eq!(bundle.hints.len(), ARITY as usize);
    let expected_len_per_seg: Vec<u32> = bundle.backend_params.iter().map(|sp| sp.reshape_row_width).collect();

    // GET /head -> parse an 8-byte LE u64
    let (status, body) = get(&app, "/head").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.len(), 8);
    let head = u64::from_le_bytes(body.as_slice().try_into().unwrap());
    assert_eq!(head, 0, "no blocks applied yet");

    // POST /answer with an encoded query bundle -> decode_response_bundle
    let mut client: RisePirClient<SimplePirBackend> = RisePirClient::from_setup(bundle, codec());
    let addr = MockFeed::address_for(0); // a real genesis address
    let (queries, _ctx) = client.build_query(&addr);
    let query_bytes = wire::encode_query_bundle(&queries);

    let (status, body) = post_answer(&app, query_bytes).await;
    assert_eq!(status, StatusCode::OK);
    let (responses, at_block) = wire::decode_response_bundle(&body, &expected_len_per_seg, ARITY as usize).expect("decode_response_bundle");
    assert_eq!(responses.len(), ARITY as usize);
    assert_eq!(at_block, 0);
}

#[tokio::test]
async fn healthz_reports_ok_and_head() {
    let (state, _feed) = build_node();
    let app = NodeState::router(state);

    let (status, body) = get(&app, "/healthz").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, b"ok 0", "fresh node: head is 0");
}

#[tokio::test]
async fn setup_concurrency_cap_is_invisible_to_sequential_clients() {
    // The `/setup` route carries a small concurrency cap (bandwidth-
    // exhaustion mitigation, threat model §3). Sequential fetches — the
    // honest-client pattern — must be entirely unaffected by it.
    let (state, _feed) = build_node();
    let app = NodeState::router(state);

    for _ in 0..3 {
        let (status, body) = get(&app, "/setup").await;
        assert_eq!(status, StatusCode::OK);
        wire::decode_setup(&body).expect("decode_setup");
    }
}

// ── (b) malformed body -> 400, never a panic ────────────────────────────

#[tokio::test]
async fn malformed_answer_body_returns_400_never_panics() {
    let (state, _feed) = build_node();
    let app = NodeState::router(state);

    let mut prng: u64 = 0x1357_9BDF_2468_ACE0;
    let mut next_byte = move || {
        prng ^= prng << 13;
        prng ^= prng >> 7;
        prng ^= prng << 17;
        (prng % 256) as u8
    };

    for len in [0usize, 1, 2, 3, 7, 16, 63, 128, 500, 4096] {
        let bytes: Vec<u8> = (0..len).map(|_| next_byte()).collect();
        let (status, _body) = post_answer(&app, bytes).await;
        // If a handler ever panicked mid-request, `oneshot` (no separate
        // task boundary) would propagate that panic straight through this
        // `.await` and fail the test with an obvious "panicked at" message
        // — so reaching this assertion at all is itself part of the "no
        // panic" proof, not just the status check.
        assert_eq!(status, StatusCode::BAD_REQUEST, "len={len}");
    }
}

// ── (c) end to end over HTTP ─────────────────────────────────────────────

enum Category {
    Live,
    Deleted,
    Never,
}

#[tokio::test]
async fn end_to_end_matches_ground_truth() {
    let (state, mut feed) = build_node();
    let app = NodeState::router(state.clone());

    // GET /setup -> RisePirClient::from_setup
    let (status, body) = get(&app, "/setup").await;
    assert_eq!(status, StatusCode::OK);
    let bundle = wire::decode_setup(&body).expect("decode_setup");
    let params = bundle.params;
    let arity = params.arity();
    let reshape_rows_per_seg: Vec<u32> = bundle.backend_params.iter().map(|sp| sp.reshape_rows).collect();
    let reshape_row_width_per_seg: Vec<u32> = bundle.backend_params.iter().map(|sp| sp.reshape_row_width).collect();
    let mut client: RisePirClient<SimplePirBackend> = RisePirClient::from_setup(bundle, codec());

    // Drive node.apply_block for ~20 mock blocks, tracking every address
    // that went to zero (a real delete, ADR-0015) along the way.
    let mut deleted: Vec<AddressHash> = Vec::new();
    for _ in 0..20 {
        let upd = feed.next_block().expect("feed").expect("mock always has a next block");
        for (addr, bal) in &upd.changes {
            if *bal == 0 {
                deleted.push(*addr);
            }
        }
        state.apply_block(&upd).await.expect("apply_block");
    }

    // GET /head, then GET /sync?from=0&to=head -> decode -> ingest_delta
    let (status, body) = get(&app, "/head").await;
    assert_eq!(status, StatusCode::OK);
    let head = u64::from_le_bytes(body.as_slice().try_into().unwrap());
    assert_eq!(head, 20);

    let (status, body) = get(&app, &format!("/sync?from=0&to={head}")).await;
    assert_eq!(status, StatusCode::OK);
    let delta = risepir_proto::codec::decode_block_delta(&body, params.plaintext_bits, arity as u32).expect("decode_block_delta");
    client.ingest_delta(&delta).expect("ingest_delta");

    // Build the category samples (after the block-driving loop, so
    // `feed.live_keys()` reflects the post-run live set).
    let mut samples: Vec<(AddressHash, Category)> = Vec::new();
    samples.extend(feed.live_keys().iter().take(6).map(|a| (*a, Category::Live)));
    samples.extend(deleted.iter().take(6).map(|a| (*a, Category::Deleted)));
    samples.extend((0..6u64).map(|i| (MockFeed::address_for(50_000_000 + i), Category::Never)));
    assert!(!deleted.is_empty(), "sanity: the run must have produced at least one delete");

    for (addr, category) in &samples {
        let (queries, ctx) = client.build_query(addr);
        let query_bytes = wire::encode_query_bundle(&queries);
        let (status, body) = post_answer(&app, query_bytes).await;
        assert_eq!(status, StatusCode::OK, "addr {addr:?}");
        let (responses, at_block) =
            wire::decode_response_bundle(&body, &reshape_row_width_per_seg, arity).expect("decode_response_bundle");
        assert_eq!(responses.len(), reshape_rows_per_seg.len());
        let result = client.finish(addr, &ctx, responses, at_block).expect("finish");

        let expected = feed.balance_of(addr);
        match category {
            // Deleted must be exactly NotFound (a real slot removal, not
            // merely "coincidentally decodes to a zero-looking balance") —
            // mirrors risepir-feed's pipeline test's own load-bearing check.
            Category::Deleted => assert_eq!(result, Lookup::NotFound, "deleted addr {addr:?} must be exactly NotFound"),
            Category::Never => assert_eq!(result, Lookup::NotFound, "never-existed addr {addr:?} must be NotFound"),
            Category::Live => {
                assert_ne!(expected, 0, "sanity: a live key has a nonzero ground-truth balance");
                match result {
                    Lookup::Found(b) => assert_eq!(b, expected, "live addr {addr:?} balance mismatch"),
                    other => panic!("live addr {addr:?} resolved to {other:?}, expected Found({expected})"),
                }
            }
        }
    }
}
