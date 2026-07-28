//! `GET /setup` Range/If-Range tests (ADR-0038).
//!
//! What has to be proven, beyond `src/node.rs`'s own unit tests for the
//! pure `parse_range` function: that the *handler* wires the parser up
//! correctly against a real cached bundle —
//!
//! (a) a matching `If-Range` unlocks a `206` whose body is the exact slice
//!     of the full body, with a correct `Content-Range`;
//! (b) two adjacent ranges reassemble byte-for-byte into the same bytes a
//!     plain `GET` returns — the property the browser client's chunked
//!     IndexedDB cache depends on to resume a download across sessions;
//! (c) a missing or mismatched `If-Range` — including one that named a
//!     bundle from *before* a regeneration (ADR-0028) — always falls back
//!     to the full `200`, never a spliced or wrong-lineage `206`;
//! (d) an out-of-bounds range is `416` with `Content-Range: bytes */total`;
//! (e) multi-range and suffix-range requests fall back to the full `200`;
//! (f) `HEAD` behaves identically to `GET` (minus the body) and never
//!     panics, since axum derives it from the same handler.

use std::sync::Arc;

use axum::body::{to_bytes, Body};
use axum::http::{header, Method, Request, StatusCode};
use tower::ServiceExt;

use ikpir_common::{SimpleConfig, SimplePirBackend};
use risepir_feed::{Feed, MockConfig, MockFeed};
use risepir_http::NodeState;
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

/// Same small mock deployment `tests/setup_cache.rs` builds, parameterised
/// on the ring's capacity for the same reason that file is: the
/// regeneration test below needs a ring small enough to actually outrun.
fn build_node(ring_capacity: usize) -> (Arc<NodeState>, MockFeed) {
    let cfg = MockConfig {
        seed: 0x5E75_ED0F_5E75_ED0F,
        num_genesis_keys: 2_000,
        changes_per_block: 40,
        inserts_per_block: 10,
        deletes_per_block: 10, // == inserts ⇒ no TableFull
    };
    let feed = MockFeed::new(cfg.clone());
    let value_codec = codec();

    let geom = Geometry::for_accounts(cfg.num_genesis_keys, ARITY, BUCKET_SIZE, FINGERPRINT_BITS, &value_codec, Backend::Simple)
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
    let state = Arc::new(NodeState::new(server, DeltaRing::new(ring_capacity), true));
    (state, feed)
}

async fn apply_blocks(state: &Arc<NodeState>, feed: &mut MockFeed, n: usize) {
    for _ in 0..n {
        let upd = feed.next_block().expect("feed").expect("mock always has a next block");
        state.apply_block(&upd).await.expect("apply_block");
    }
}

struct RangeResp {
    status: StatusCode,
    body: Vec<u8>,
    content_range: Option<String>,
    accept_ranges: Option<String>,
    etag: Option<String>,
}

async fn setup_request(app: &axum::Router, method: Method, range: Option<&str>, if_range: Option<&str>) -> RangeResp {
    let mut req = Request::builder().method(method).uri("/setup");
    if let Some(r) = range {
        req = req.header(header::RANGE, r);
    }
    if let Some(ir) = if_range {
        req = req.header(header::IF_RANGE, ir);
    }
    let resp = app.clone().oneshot(req.body(Body::empty()).unwrap()).await.unwrap();
    let status = resp.status();
    let h = resp.headers();
    let content_range = h.get(header::CONTENT_RANGE).and_then(|v| v.to_str().ok()).map(str::to_owned);
    let accept_ranges = h.get(header::ACCEPT_RANGES).and_then(|v| v.to_str().ok()).map(str::to_owned);
    let etag = h.get(header::ETAG).and_then(|v| v.to_str().ok()).map(str::to_owned);
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap().to_vec();
    RangeResp {
        status,
        body,
        content_range,
        accept_ranges,
        etag,
    }
}

async fn full_get(app: &axum::Router) -> RangeResp {
    setup_request(app, Method::GET, None, None).await
}

// ── (a) a matching If-Range unlocks a correct 206 ───────────────────────

#[tokio::test]
async fn matching_if_range_yields_206_with_the_exact_slice() {
    let (state, _feed) = build_node(300);
    let app = NodeState::router(state.clone());

    let full = full_get(&app).await;
    assert_eq!(full.status, StatusCode::OK);
    assert_eq!(full.accept_ranges.as_deref(), Some("bytes"), "Accept-Ranges must be on the 200 too");
    let etag = full.etag.clone().expect("ETag on the full response");
    let total = full.body.len();
    assert!(total > 1000, "sanity: the mock bundle is not tiny");

    let mid = setup_request(&app, Method::GET, Some("bytes=10-99"), Some(&etag)).await;
    assert_eq!(mid.status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(mid.body, full.body[10..=99]);
    assert_eq!(mid.content_range.as_deref(), Some(format!("bytes 10-99/{total}").as_str()));
    assert_eq!(mid.accept_ranges.as_deref(), Some("bytes"));
    assert_eq!(mid.etag.as_deref(), Some(etag.as_str()), "a 206 must carry the same ETag as the 200 it slices");

    // Open-ended, reaching the true end.
    let tail_from = total - 50;
    let tail = setup_request(&app, Method::GET, Some(&format!("bytes={tail_from}-")), Some(&etag)).await;
    assert_eq!(tail.status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(tail.body, full.body[tail_from..]);
    assert_eq!(
        tail.content_range.as_deref(),
        Some(format!("bytes {tail_from}-{}/{total}", total - 1).as_str())
    );

    // An over-long last-byte-pos clamps to the true end rather than erroring.
    let clamped = setup_request(&app, Method::GET, Some(&format!("bytes=0-{}", total * 10)), Some(&etag)).await;
    assert_eq!(clamped.status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(clamped.body, full.body);
    assert_eq!(clamped.content_range.as_deref(), Some(format!("bytes 0-{}/{total}", total - 1).as_str()));
}

// ── (b) two adjacent ranges reassemble the full body exactly ───────────

#[tokio::test]
async fn concatenating_two_ranges_reproduces_the_full_body_byte_for_byte() {
    let (state, _feed) = build_node(300);
    let app = NodeState::router(state.clone());

    let full = full_get(&app).await;
    let etag = full.etag.clone().unwrap();
    let total = full.body.len();
    let mid = total / 2;

    let first_half = setup_request(&app, Method::GET, Some(&format!("bytes=0-{}", mid - 1)), Some(&etag)).await;
    let second_half = setup_request(&app, Method::GET, Some(&format!("bytes={mid}-")), Some(&etag)).await;
    assert_eq!(first_half.status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(second_half.status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(first_half.body.len(), mid);
    assert_eq!(second_half.body.len(), total - mid);

    let mut reassembled = first_half.body;
    reassembled.extend_from_slice(&second_half.body);
    assert_eq!(reassembled, full.body, "two adjacent ranges must reassemble byte-for-byte into the full body");

    // Three overlapping pieces (as a resumed download's own retries might
    // produce, if a stall lands mid-chunk) must reassemble just as
    // cleanly once the overlap is deduplicated by the caller.
    let a = setup_request(&app, Method::GET, Some("bytes=0-99"), Some(&etag)).await;
    let b = setup_request(&app, Method::GET, Some("bytes=50-199"), Some(&etag)).await;
    assert_eq!(a.body[50..], b.body[..50]);
}

// ── (c) missing/mismatched If-Range always falls back to the full 200 ──

#[tokio::test]
async fn missing_or_mismatched_if_range_serves_the_full_200() {
    let (state, _feed) = build_node(300);
    let app = NodeState::router(state.clone());

    let full = full_get(&app).await;

    // Range present, If-Range absent entirely.
    let no_if_range = setup_request(&app, Method::GET, Some("bytes=0-9"), None).await;
    assert_eq!(no_if_range.status, StatusCode::OK);
    assert_eq!(no_if_range.body, full.body);

    // Range present, If-Range present but wrong.
    let wrong_if_range = setup_request(
        &app,
        Method::GET,
        Some("bytes=0-9"),
        Some("\"setup-deadbeefdeadbeef-999999\""),
    )
    .await;
    assert_eq!(wrong_if_range.status, StatusCode::OK);
    assert_eq!(wrong_if_range.body, full.body);

    // Junk If-Range that fails even to decode meaningfully.
    let junk_if_range = setup_request(&app, Method::GET, Some("bytes=0-9"), Some("not an etag at all")).await;
    assert_eq!(junk_if_range.status, StatusCode::OK);
    assert_eq!(junk_if_range.body, full.body);
}

/// THE hazard the mandatory `If-Range` exists to close (ADR-0028's splice
/// risk): a client holds an `ETag` from an earlier regeneration, the cache
/// regenerates under the pressure of the head advancing (same epoch, a
/// later pinned block, genuinely different bytes), and the client's now-
/// stale `If-Range` must never unlock a `206` against the new bundle.
#[tokio::test]
async fn a_stale_if_range_from_before_a_regeneration_never_unlocks_a_206() {
    // A tiny ring forces a regeneration quickly — same technique as
    // `tests/setup_cache.rs`'s own staleness test.
    let (state, mut feed) = build_node(16);
    let app = NodeState::router(state.clone());

    let first = full_get(&app).await;
    let stale_etag = first.etag.clone().expect("ETag on the first response");

    // Past the fresh window (an eighth of a 16-block ring = 2 blocks).
    apply_blocks(&state, &mut feed, 15).await;

    let fresh = full_get(&app).await;
    assert_ne!(fresh.etag, first.etag, "sanity: the bundle really did regenerate under the same epoch");

    let resumed = setup_request(&app, Method::GET, Some("bytes=0-9"), Some(&stale_etag)).await;
    assert_eq!(
        resumed.status,
        StatusCode::OK,
        "a stale If-Range must never unlock a 206 against a regenerated (and therefore different) bundle"
    );
    assert_eq!(resumed.body, fresh.body, "the fallback must be the CURRENT full body, not the stale one");
}

// ── (d) an out-of-bounds range is 416 ────────────────────────────────────

#[tokio::test]
async fn unsatisfiable_range_is_416_with_content_range_star() {
    let (state, _feed) = build_node(300);
    let app = NodeState::router(state.clone());

    let full = full_get(&app).await;
    let etag = full.etag.clone().unwrap();
    let total = full.body.len();

    let at_total = setup_request(&app, Method::GET, Some(&format!("bytes={total}-")), Some(&etag)).await;
    assert_eq!(at_total.status, StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(at_total.content_range.as_deref(), Some(format!("bytes */{total}").as_str()));
    assert!(at_total.body.is_empty());

    let past_total = setup_request(&app, Method::GET, Some(&format!("bytes={}-", total + 1_000_000)), Some(&etag)).await;
    assert_eq!(past_total.status, StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(past_total.content_range.as_deref(), Some(format!("bytes */{total}").as_str()));
}

// ── (e) multi-range and suffix-range fall back to the full 200 ─────────

#[tokio::test]
async fn multi_range_and_suffix_range_fall_back_to_full_200() {
    let (state, _feed) = build_node(300);
    let app = NodeState::router(state.clone());

    let full = full_get(&app).await;
    let etag = full.etag.clone().unwrap();

    let multi = setup_request(&app, Method::GET, Some("bytes=0-9,20-29"), Some(&etag)).await;
    assert_eq!(multi.status, StatusCode::OK);
    assert_eq!(multi.body, full.body);

    let suffix = setup_request(&app, Method::GET, Some("bytes=-500"), Some(&etag)).await;
    assert_eq!(suffix.status, StatusCode::OK);
    assert_eq!(suffix.body, full.body);

    let garbage = setup_request(&app, Method::GET, Some("total nonsense"), Some(&etag)).await;
    assert_eq!(garbage.status, StatusCode::OK);
    assert_eq!(garbage.body, full.body);
}

// ── (f) HEAD behaves and never panics ───────────────────────────────────

#[tokio::test]
async fn head_with_range_behaves_like_get_minus_the_body() {
    let (state, _feed) = build_node(300);
    let app = NodeState::router(state.clone());

    let full = full_get(&app).await;
    let etag = full.etag.clone().unwrap();
    let total = full.body.len();

    let head_partial = setup_request(&app, Method::HEAD, Some("bytes=10-99"), Some(&etag)).await;
    assert_eq!(head_partial.status, StatusCode::PARTIAL_CONTENT);
    assert!(head_partial.body.is_empty(), "axum strips the body for HEAD");
    assert_eq!(head_partial.content_range.as_deref(), Some(format!("bytes 10-99/{total}").as_str()));
    assert_eq!(head_partial.accept_ranges.as_deref(), Some("bytes"));

    let head_full = setup_request(&app, Method::HEAD, None, None).await;
    assert_eq!(head_full.status, StatusCode::OK);
    assert!(head_full.body.is_empty());
    assert_eq!(head_full.accept_ranges.as_deref(), Some("bytes"));
    assert_eq!(head_full.content_range, None, "no Content-Range on a full response");

    let head_416 = setup_request(&app, Method::HEAD, Some(&format!("bytes={total}-")), Some(&etag)).await;
    assert_eq!(head_416.status, StatusCode::RANGE_NOT_SATISFIABLE);
    assert!(head_416.body.is_empty());
    assert_eq!(head_416.content_range.as_deref(), Some(format!("bytes */{total}").as_str()));
}

// ── /head carries x-risepir-mode too (ADR-0038) ─────────────────────────

#[tokio::test]
async fn head_endpoint_carries_epoch_and_mode() {
    let (state, _feed) = build_node(300);
    let app = NodeState::router(state.clone());

    let resp = app
        .clone()
        .oneshot(Request::builder().uri("/head").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.headers().get("x-risepir-epoch").unwrap(), state.epoch());
    assert_eq!(
        resp.headers().get("x-risepir-mode").unwrap(),
        "1",
        "this mock deployment is complete, and /head must say so without a /setup round trip"
    );
}
