//! `GET /setup` response-cache tests (ADR-0028).
//!
//! The cache exists so the server encodes one shared `Bytes` and hands
//! every client a refcounted clone, instead of deep-cloning ~831 MB of
//! hints and encoding another ~831 MB per request. What has to be proven
//! is not that it is fast — it is that it is still *correct*:
//!
//! (a) the encode happens once and every client gets byte-identical bytes;
//! (b) a cached bundle is never served once the delta ring can no longer
//!     bridge it forward — the wrong-answer path this whole feature has to
//!     avoid, since a client that bootstraps at a block the server cannot
//!     `/sync` from is stranded;
//! (c) a client bootstrapped from a *cached* (deliberately older) bundle
//!     still answers byte-exactly after syncing forward;
//! (d) the `ETag` / `If-None-Match` revalidation path, including junk
//!     input — `If-None-Match` is attacker-controlled and must never panic.

use std::sync::Arc;

use axum::body::{to_bytes, Body};
use axum::http::{header, Request, StatusCode};
use tower::ServiceExt;

use ikpir_common::{SimpleConfig, SimplePirBackend};
use risepir_client::{Lookup, RisePirClient};
use risepir_feed::{Feed, MockConfig, MockFeed};
use risepir_http::{wire, NodeState};
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

/// Same small mock deployment `tests/http.rs` builds, with the delta
/// ring's capacity left to the caller: the cache's freshness rule is
/// stated in terms of that capacity, so a test that wants to watch a
/// cached bundle go stale needs a ring it can actually outrun.
fn build_node(ring_capacity: usize) -> (Arc<NodeState>, MockFeed) {
    let cfg = MockConfig {
        seed: 0xA11C_E999_0BAD_F00D,
        num_genesis_keys: 2_000,
        changes_per_block: 40,
        inserts_per_block: 10,
        deletes_per_block: 10, // == inserts ⇒ live-set size invariant ⇒ no TableFull
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
    let state = Arc::new(NodeState::new(server, DeltaRing::new(ring_capacity), true));
    (state, feed)
}

/// `GET uri`, returning status, the `ETag` header (if any), and the body.
async fn get_with(app: &axum::Router, uri: &str, if_none_match: Option<&str>) -> (StatusCode, Option<String>, Vec<u8>) {
    let mut req = Request::builder().uri(uri);
    if let Some(inm) = if_none_match {
        req = req.header(header::IF_NONE_MATCH, inm);
    }
    let resp = app.clone().oneshot(req.body(Body::empty()).unwrap()).await.unwrap();
    let status = resp.status();
    let etag = resp
        .headers()
        .get(header::ETAG)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap().to_vec();
    (status, etag, body)
}

async fn get(app: &axum::Router, uri: &str) -> (StatusCode, Vec<u8>) {
    let (status, _, body) = get_with(app, uri, None).await;
    (status, body)
}

async fn head_block(app: &axum::Router) -> u64 {
    let (status, body) = get(app, "/head").await;
    assert_eq!(status, StatusCode::OK);
    u64::from_le_bytes(body.as_slice().try_into().unwrap())
}

async fn apply_blocks(state: &Arc<NodeState>, feed: &mut MockFeed, n: usize) {
    for _ in 0..n {
        let upd = feed.next_block().expect("feed").expect("mock always has a next block");
        state.apply_block(&upd).await.expect("apply_block");
    }
}

// ── (a) one encode, shared bytes ────────────────────────────────────────

#[tokio::test]
async fn setup_is_encoded_once_and_served_byte_identically() {
    let (state, _feed) = build_node(300);
    let app = NodeState::router(state.clone());

    assert_eq!(state.setup_generation(), 0, "nothing encoded before the first request");

    let (s1, e1, b1) = get_with(&app, "/setup", None).await;
    let (s2, e2, b2) = get_with(&app, "/setup", None).await;
    let (s3, e3, b3) = get_with(&app, "/setup", None).await;

    assert_eq!(s1, StatusCode::OK);
    assert_eq!(s2, StatusCode::OK);
    assert_eq!(s3, StatusCode::OK);
    assert_eq!(b1, b2, "two clients must receive byte-identical setup bundles");
    assert_eq!(b2, b3);
    assert_eq!(e1, e2);
    assert_eq!(e2, e3);
    assert_eq!(
        e1.as_deref(),
        Some(format!("\"setup-{}-0\"", state.epoch()).as_str()),
        "ETag names the lineage epoch and the pinned block (ADR-0033)"
    );

    assert_eq!(
        state.setup_generation(),
        1,
        "the bundle must be encoded exactly once and then shared, not re-encoded per request"
    );

    // And the shared bytes are a real, decodable bundle, not a cached husk.
    let bundle = wire::decode_setup(&b1).expect("decode_setup");
    assert_eq!(bundle.hints.len(), ARITY as usize);
    assert_eq!(bundle.block, 0);
}

// ── (b) the failure mode: never serve a bundle the ring cannot bridge ────

#[tokio::test]
async fn a_stale_cache_regenerates_and_what_it_serves_is_still_bridgeable() {
    // Ring of 8 ⇒ the cache is reused only while the head is within 4
    // blocks of the cached bundle (half the window; see `setup_bytes`).
    let (state, mut feed) = build_node(8);
    let app = NodeState::router(state.clone());

    let (status, _, first) = get_with(&app, "/setup", None).await;
    assert_eq!(status, StatusCode::OK);
    let first_block = wire::decode_setup(&first).expect("decode_setup").block;
    assert_eq!(first_block, 0);
    assert_eq!(state.setup_generation(), 1);

    // Still inside the half-window: the identical bytes must come back,
    // with no second encode. (Non-vacuity: if the rule were "regenerate
    // always", the assertion below would fail here rather than later.)
    apply_blocks(&state, &mut feed, 3).await;
    let (_, _, still_cached) = get_with(&app, "/setup", None).await;
    assert_eq!(still_cached, first, "a fresh-enough cache must be reused verbatim");
    assert_eq!(state.setup_generation(), 1);

    // Past the half-window: a *new* bundle, pinned at a newer block.
    apply_blocks(&state, &mut feed, 6).await;
    let (status, _, second) = get_with(&app, "/setup", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_ne!(second, first, "a stale cache must not be served");
    assert_eq!(state.setup_generation(), 2, "exactly one regeneration");
    let second_block = wire::decode_setup(&second).expect("decode_setup").block;
    assert!(second_block > first_block, "the fresh bundle must be pinned later");

    // The property that actually matters: a client bootstrapping from what
    // was just served can still be bridged forward. Anything else strands
    // it — it would hold a hint the server cannot `/sync` it out of.
    //
    // One more block first, deliberately: a bundle pinned at the current
    // head needs no sync at all, and `DeltaRing::range` reports an empty
    // `(from, to]` as `409` by definition ("nothing to coalesce"), so
    // asking at `from == to` would test the ring's degenerate case rather
    // than this cache's freshness rule.
    apply_blocks(&state, &mut feed, 1).await;
    let head = head_block(&app).await;
    assert!(head > second_block);
    let (status, _) = get(&app, &format!("/sync?from={second_block}&to={head}&epoch={}", state.epoch())).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the block /setup just served must still be inside the retained delta window"
    );

    // And the stale one genuinely is not bridgeable any more — which is
    // exactly why it had to be regenerated.
    let (status, _) = get(&app, &format!("/sync?from={first_block}&to={head}&epoch={}", state.epoch())).await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "sanity: the superseded bundle's block has aged out, so serving it would have stranded a client"
    );
}

// ── (c) correctness through a cached bundle ─────────────────────────────

#[tokio::test]
async fn a_client_bootstrapped_from_a_cached_bundle_still_answers_exactly() {
    let (state, mut feed) = build_node(300);
    let app = NodeState::router(state.clone());

    // Prime the cache at block 0, then let the server move on. The next
    // client gets the *cached* bundle — pinned in the past — which is the
    // whole point of the feature and the thing that could break the rewind.
    let (status, _, _) = get_with(&app, "/setup", None).await;
    assert_eq!(status, StatusCode::OK);
    apply_blocks(&state, &mut feed, 20).await;

    let (status, _, body) = get_with(&app, "/setup", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(state.setup_generation(), 1, "still the block-0 bundle, served from cache");
    let bundle = wire::decode_setup(&body).expect("decode_setup");
    assert_eq!(bundle.block, 0, "deliberately bootstrapping from a stale-but-bridgeable bundle");

    let params = bundle.params;
    let arity = params.arity();
    let reshape_row_width_per_seg: Vec<u32> = bundle.backend_params.iter().map(|sp| sp.reshape_row_width).collect();
    let mut client: RisePirClient<SimplePirBackend> = RisePirClient::from_setup(bundle, codec());

    // Sync the client from the cached bundle's block up to the head.
    let head = head_block(&app).await;
    let (status, body) = get(&app, &format!("/sync?from=0&to={head}&epoch={}", state.epoch())).await;
    assert_eq!(status, StatusCode::OK);
    let delta =
        risepir_proto::codec::decode_block_delta(&body, params.plaintext_bits, arity as u32).expect("decode_block_delta");
    client.ingest_delta(&delta).expect("ingest_delta");

    // Every live account must come back byte-exact against the mock's own
    // ground truth. A cache that broke the rewind would show up here.
    let live: Vec<_> = feed.live_keys().iter().take(8).copied().collect();
    assert!(!live.is_empty(), "sanity: the mock must have live accounts");
    for addr in live {
        let expected = feed.balance_of(&addr);
        let (queries, ctx) = client.build_query(&addr);
        let (status, body) = {
            let resp = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri(format!("/answer?epoch={}", state.epoch()))
                        .body(Body::from(wire::encode_query_bundle(&queries)))
                        .unwrap(),
                )
                .await
                .unwrap();
            let s = resp.status();
            (s, to_bytes(resp.into_body(), usize::MAX).await.unwrap().to_vec())
        };
        assert_eq!(status, StatusCode::OK);
        let (responses, at_block) =
            wire::decode_response_bundle(&body, &reshape_row_width_per_seg, arity).expect("decode_response_bundle");
        match client.finish(&addr, &ctx, responses, at_block).expect("finish") {
            Lookup::Found(balance) => assert_eq!(balance, expected, "balance for {addr:?} through a cached bundle"),
            other => panic!("expected Found({expected}) for a live account, got {other:?}"),
        }
    }
}

// ── (d) ETag revalidation, including hostile input ──────────────────────

#[tokio::test]
async fn if_none_match_revalidates_and_junk_never_panics() {
    let (state, _feed) = build_node(300);
    let app = NodeState::router(state.clone());

    let (status, etag, full) = get_with(&app, "/setup", None).await;
    assert_eq!(status, StatusCode::OK);
    let etag = etag.expect("/setup must carry an ETag");
    assert!(!full.is_empty());

    // A matching validator: 304, no body, and no re-encode.
    let (status, etag_304, body) = get_with(&app, "/setup", Some(&etag)).await;
    assert_eq!(status, StatusCode::NOT_MODIFIED);
    assert!(body.is_empty(), "a 304 must not carry a body");
    assert_eq!(etag_304.as_deref(), Some(etag.as_str()));
    assert_eq!(state.setup_generation(), 1);

    // Anything else must serve the full bundle rather than a wrong 304 —
    // a spurious 304 would leave a client with no hint at all. Every one
    // of these is attacker-controlled request input.
    for junk in [
        "\"setup-999999\"",         // a validator for a bundle we are not serving
        "\"\"",                     // empty quoted
        "",                         // empty
        "*",                        // the wildcard: matches "any current representation"
        "setup-0",                  // unquoted
        "W/\"setup-0\"",            // weak validator
        "\"setup-0",                // unterminated quote
        "garbage, \"setup-0\"",     // list form, one entry matching
        "\u{feff}\"setup-0\"",      // leading BOM
        &"\"".repeat(4096),         // long, degenerate
    ] {
        let (status, _, body) = get_with(&app, "/setup", Some(junk)).await;
        assert!(
            status == StatusCode::OK || status == StatusCode::NOT_MODIFIED,
            "If-None-Match {junk:?} must be handled cleanly, got {status}"
        );
        if status == StatusCode::OK {
            assert_eq!(body, full, "a non-matching validator must serve the current bundle verbatim");
        } else {
            assert!(body.is_empty());
        }
    }

    // Whatever the junk did, it must not have caused a re-encode.
    assert_eq!(state.setup_generation(), 1);
}
