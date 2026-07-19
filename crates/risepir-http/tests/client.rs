//! Real-socket integration test for [`risepir_http::PirHttpClient`]
//! (Stage 0.4 Part A): unlike `tests/http.rs` (which drives the router
//! in-process via `tower::oneshot`, no real network), this binds an actual
//! ephemeral TCP port, serves [`NodeState::router`] on it, and drives
//! every `PirHttpClient` method over genuine HTTP — the exact path
//! `risepir-rpc`'s JSON-RPC front end depends on.

use std::sync::Arc;

use ikpir_common::{SimpleConfig, SimplePirBackend};
use risepir_client::{Lookup, RisePirClient};
use risepir_feed::{Feed, MockConfig, MockFeed};
use risepir_http::{NodeState, PirHttpClient};
use risepir_proto::{Backend, Geometry, ValueCodec};
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

/// Same shape as `tests/http.rs`'s own `build_node`, plus actually binding
/// and serving on a real (ephemeral) TCP port and returning the base URL a
/// [`PirHttpClient`] can reach it at.
async fn spawn_node() -> (String, MockFeed) {
    let cfg = MockConfig {
        seed: 0xC0FF_EE00_1234_5678,
        num_genesis_keys: 1_500,
        changes_per_block: 30,
        inserts_per_block: 8,
        deletes_per_block: 8,
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
    let router = NodeState::router(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local_addr");
    tokio::spawn(async move {
        axum::serve(listener, router).await.expect("axum::serve");
    });

    (format!("http://{addr}"), feed)
}

/// `setup`/`head`/`answer` round trip over a real socket: bootstrap a
/// `RisePirClient` from `PirHttpClient::setup`, confirm `head() == 0`
/// before any block, then complete one full private lookup for a genesis
/// address via `PirHttpClient::answer` and check it matches ground truth.
#[tokio::test]
async fn setup_head_and_answer_round_trip_over_real_http() {
    let (base, feed) = spawn_node().await;
    let pir = PirHttpClient::new(&base);

    let bundle = pir.setup().await.expect("GET /setup");
    assert_eq!(bundle.params.arity(), ARITY as usize);
    assert_eq!(bundle.block, 0);
    let reshape_row_width_per_seg: Vec<u32> = bundle.backend_params.iter().map(|sp| sp.reshape_row_width).collect();
    let arity = bundle.params.arity();

    assert_eq!(pir.head().await.expect("GET /head"), 0, "no blocks applied yet");

    let mut client: RisePirClient<SimplePirBackend> = RisePirClient::from_setup(bundle, codec());
    let addr = MockFeed::address_for(0); // a real genesis address
    let (queries, ctx) = client.build_query(&addr);

    let (responses, at_block) = pir.answer(&queries, &reshape_row_width_per_seg, arity).await.expect("POST /answer");
    assert_eq!(at_block, 0);
    let result = client.finish(&addr, &ctx, responses, at_block).expect("finish");
    assert_eq!(result, Lookup::Found(feed.balance_of(&addr)));
}

/// `sync` over a real socket: apply a handful of blocks directly against
/// the server's underlying `NodeState` (mirroring the still-out-of-scope
/// block-following driver), then pull the coalesced delta over HTTP via
/// `PirHttpClient::sync`, ingest it into a freshly-bootstrapped client, and
/// confirm a post-genesis private lookup matches ground truth — proving
/// `sync`'s `plaintext_bits`/`arity` decode plumbing is wired correctly
/// end to end, not just unit-tested against synthetic bytes.
#[tokio::test]
async fn sync_pulls_a_real_delta_over_http() {
    // A second, independent node — this test drives its own block stream
    // directly against a `NodeState`, so it needs a handle to that state,
    // which `spawn_node` deliberately does not return (the point of this
    // test file is the `PirHttpClient` surface, not `NodeState` itself —
    // `tests/http.rs` already covers driving `apply_block` in depth). So
    // this test builds its own small node inline instead of reusing
    // `spawn_node`, keeping a `state` handle to advance blocks with.
    let cfg = MockConfig {
        seed: 0xFEED_1234_5678_9ABC,
        num_genesis_keys: 800,
        changes_per_block: 20,
        inserts_per_block: 5,
        deletes_per_block: 5,
    };
    let mut feed = MockFeed::new(cfg.clone());
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
    let router = NodeState::router(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local_addr");
    tokio::spawn(async move {
        axum::serve(listener, router).await.expect("axum::serve");
    });
    let base = format!("http://{addr}");
    let pir = PirHttpClient::new(&base);

    let bundle = pir.setup().await.expect("GET /setup");
    let plaintext_bits = bundle.params.plaintext_bits;
    let arity = bundle.params.arity();
    let reshape_row_width_per_seg: Vec<u32> = bundle.backend_params.iter().map(|sp| sp.reshape_row_width).collect();
    let mut client: RisePirClient<SimplePirBackend> = RisePirClient::from_setup(bundle, codec());

    for _ in 0..10 {
        let upd = feed.next_block().expect("feed").expect("mock always has a next block");
        state.apply_block(&upd).await.expect("apply_block");
    }
    let head = pir.head().await.expect("GET /head");
    assert_eq!(head, 10);

    let delta = pir
        .sync(0, head, plaintext_bits, arity as u32)
        .await
        .expect("GET /sync")
        .expect("sync must be Some: within the retention window");
    client.ingest_delta(&delta).expect("ingest_delta");

    // A live post-genesis address: query it privately and check it agrees
    // with the mock's exact ground truth.
    let live_addr = *feed.live_keys().first().expect("at least one live key");
    let (queries, ctx) = client.build_query(&live_addr);
    let (responses, at_block) = pir.answer(&queries, &reshape_row_width_per_seg, arity).await.expect("POST /answer");
    let result = client.finish(&live_addr, &ctx, responses, at_block).expect("finish");
    assert_eq!(result, Lookup::Found(feed.balance_of(&live_addr)));

    // Out-of-window sync (from a block older than genesis's own history
    // could ever retain, i.e. asking for a range this ring never held)
    // must map to `Ok(None)`, never an `Err` or a fabricated delta — the
    // ring here has capacity 300 and only 10 blocks have ever been
    // pushed, so `to=200` (beyond the current head) exercises the
    // "requested range outside the retained window" 409 path.
    let out_of_window = pir.sync(0, 200, plaintext_bits, arity as u32).await.expect("GET /sync (out of window)");
    assert_eq!(out_of_window, None, "409 must map to Ok(None), never Err or a fabricated delta");
}

/// `PirHttpClient::head`/`setup`/`answer` against a server that returns a
/// non-2xx status (a malformed `/answer` body triggers the server's own
/// `400 Bad Request`) must surface as `ClientError::Status`, never a panic
/// and never silently treated as success.
#[tokio::test]
async fn non_200_status_surfaces_as_client_error() {
    let (base, _feed) = spawn_node().await;
    let pir = PirHttpClient::new(&base);

    let err = pir
        .answer(&[], &[], 0)
        .await
        .expect_err("an empty query bundle mismatches this deployment's arity and must 400");
    match err {
        risepir_http::ClientError::Status { status, .. } => assert_eq!(status, 400),
        other => panic!("expected ClientError::Status, got {other:?}"),
    }
}
