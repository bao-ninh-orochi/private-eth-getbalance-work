//! `GET /metrics` (ADR-0039), driven the same way `tests/http.rs` drives
//! everything else: `tower::ServiceExt::oneshot`, no real socket.
//!
//! What is pinned here is not "the numbers look plausible" (that is what
//! `crate::metrics`'s own unit tests, run against hand-built fixtures,
//! already cover) but the properties that would be silently wrong if this
//! integration ever drifted from that unit-level contract: that the real,
//! router-served body still parses as well-formed Prometheus exposition
//! text, that the histogram's bucket counts (as actually rendered from
//! real `/answer` traffic) stay monotonic and its `_count` matches the
//! number of `/answer` calls this test made, that per-route/error-class
//! counters reflect real request outcomes, and — the privacy tripwire —
//! that nothing in the real served body looks address- or ciphertext-
//! segment-shaped.

use std::sync::Arc;

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use ikpir_common::{SimpleConfig, SimplePirBackend};
use risepir_client::RisePirClient;
use risepir_http::{wire, NodeState};
use risepir_proto::{keccak256, Backend, Geometry, ValueCodec};
use risepir_server::{DeltaRing, RisePirServer};
use segmented_cuckoo::{Segmented2aryCuckooKVStore, Segmented2aryScheme};

const ARITY: u32 = 2;
const BUCKET_SIZE: u32 = 4;
const FINGERPRINT_BITS: u32 = 32;
const LWE_DIM: u32 = 256;

fn codec() -> ValueCodec {
    ValueCodec {
        key_tag_bits: 32,
        balance_bits: 96,
        checksum_bits: 16,
    }
}

/// A small, self-contained node with one seeded account — this test only
/// needs `/answer` to succeed and (separately) to fail, never a ground-
/// truth comparison, so there is no `MockFeed` here (unlike `tests/http.rs`).
fn build_node() -> Arc<NodeState> {
    let value_codec = codec();
    let geom = Geometry::for_accounts(
        200,
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
    let key = keccak256(&[0x11u8; 20]);
    let v = value_codec.encode(&key, 42).expect("encode seeded account");
    store.insert(key, &v).expect("insert seeded account");

    let server: RisePirServer<Segmented2aryScheme, SimplePirBackend> =
        RisePirServer::new(store, SimpleConfig::with_lwe_dim(LWE_DIM), value_codec, 0);
    Arc::new(NodeState::new(server, DeltaRing::new(64), true))
}

async fn get(app: &axum::Router, uri: &str) -> (StatusCode, Vec<u8>, axum::http::HeaderMap) {
    let resp = app
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let body = to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap()
        .to_vec();
    (status, body, headers)
}

async fn post_answer(app: &axum::Router, epoch: &str, body: Vec<u8>) -> StatusCode {
    app.clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/answer?epoch={epoch}"))
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap()
        .status()
}

// ─── a minimal, hand-rolled Prometheus-text well-formedness checker ─────
//
// Deliberately not a general-purpose parser (this repo takes no new
// dependency for the writer either — `crate::metrics`'s own module docs —
// so a test-only parser crate would be an odd asymmetry): just enough
// structure checking to catch the failure modes that matter — a metric
// sampled with no preceding `# HELP`/`# TYPE`, an unparseable value, or an
// unterminated label block.

struct Sample {
    name: String,
    labels: Vec<(String, String)>,
    value: f64,
}

fn parse_and_check(text: &str) -> Vec<Sample> {
    let mut declared_help = std::collections::HashSet::new();
    let mut declared_type = std::collections::HashMap::new();
    let mut samples = Vec::new();

    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("# HELP ") {
            let name = rest
                .split_whitespace()
                .next()
                .unwrap_or_else(|| panic!("HELP line names no metric: {line:?}"));
            declared_help.insert(name.to_string());
            continue;
        }
        if let Some(rest) = line.strip_prefix("# TYPE ") {
            let mut it = rest.split_whitespace();
            let name = it
                .next()
                .unwrap_or_else(|| panic!("TYPE line names no metric: {line:?}"));
            let ty = it
                .next()
                .unwrap_or_else(|| panic!("TYPE line names no type: {line:?}"));
            assert!(
                ["gauge", "counter", "histogram"].contains(&ty),
                "unknown metric type {ty:?}: {line:?}"
            );
            declared_type.insert(name.to_string(), ty.to_string());
            continue;
        }
        if line.is_empty() {
            continue;
        }
        assert!(
            !line.starts_with('#'),
            "unrecognised comment line: {line:?}"
        );

        let (head, value_str) = line
            .rsplit_once(' ')
            .unwrap_or_else(|| panic!("no value on line: {line:?}"));
        let value: f64 = value_str
            .parse()
            .unwrap_or_else(|_| panic!("value {value_str:?} does not parse as a number: {line:?}"));

        let (name, labels) = match head.split_once('{') {
            None => (head.to_string(), Vec::new()),
            Some((name, rest)) => {
                let inner = rest
                    .strip_suffix('}')
                    .unwrap_or_else(|| panic!("unterminated label block: {line:?}"));
                let labels = inner
                    .split(',')
                    .map(|kv| {
                        let (k, v) = kv
                            .split_once('=')
                            .unwrap_or_else(|| panic!("malformed label {kv:?} in {line:?}"));
                        let v = v
                            .strip_prefix('"')
                            .and_then(|v| v.strip_suffix('"'))
                            .unwrap_or_else(|| {
                                panic!("label value not quoted: {kv:?} in {line:?}")
                            });
                        (k.to_string(), v.to_string())
                    })
                    .collect();
                (name.to_string(), labels)
            }
        };
        assert!(!name.is_empty(), "empty metric name: {line:?}");

        let base = name
            .strip_suffix("_bucket")
            .or_else(|| name.strip_suffix("_sum"))
            .or_else(|| name.strip_suffix("_count"))
            .unwrap_or(&name);
        assert!(
            declared_help.contains(base),
            "{name} sampled with no preceding # HELP: {line:?}"
        );
        assert!(
            declared_type.contains_key(base),
            "{name} sampled with no preceding # TYPE: {line:?}"
        );

        samples.push(Sample {
            name,
            labels,
            value,
        });
    }
    samples
}

fn find<'a>(samples: &'a [Sample], name: &str) -> Vec<&'a Sample> {
    samples.iter().filter(|s| s.name == name).collect()
}
fn label<'a>(s: &'a Sample, k: &str) -> Option<&'a str> {
    s.labels
        .iter()
        .find(|(lk, _)| lk == k)
        .map(|(_, v)| v.as_str())
}

// ─── tests ───────────────────────────────────────────────────────────────

#[tokio::test]
async fn metrics_body_is_well_formed_prometheus_text() {
    let state = build_node();
    let app = NodeState::router(state);

    let (status, body, headers) = get(&app, "/metrics").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers
            .get(axum::http::header::CONTENT_TYPE)
            .unwrap()
            .to_str()
            .unwrap(),
        "text/plain; version=0.0.4; charset=utf-8"
    );
    assert_eq!(
        headers
            .get(axum::http::header::CACHE_CONTROL)
            .unwrap()
            .to_str()
            .unwrap(),
        "no-store"
    );

    let text = String::from_utf8(body).expect("metrics body must be UTF-8 text");
    let samples = parse_and_check(&text); // panics on any structural defect
    assert!(
        !samples.is_empty(),
        "a fresh node must still emit its gauges"
    );

    // A handful of the always-present gauges, by name — every one of these
    // must exist even on a node that has served zero requests, mirroring
    // /healthz's own "a field is always present, never omitted" rule.
    for name in [
        "risepir_head_block",
        "risepir_finalized_block",
        "risepir_block_lag",
        "risepir_store_items",
        "risepir_store_capacity",
        "risepir_build_info",
        "risepir_process_uptime_seconds",
        "risepir_reconcile_configured",
        "risepir_state_save_configured",
    ] {
        assert!(
            !find(&samples, name).is_empty(),
            "missing always-present metric {name}"
        );
    }

    // GET /healthz stays byte-for-byte the constraint it always was —
    // /metrics is additive, never a replacement.
    let (hz_status, hz_body, _) = get(&app, "/healthz").await;
    assert_eq!(hz_status, StatusCode::OK);
    assert!(String::from_utf8(hz_body).unwrap().starts_with("ok 0\n"));
}

/// The load-bearing shape check: buckets must be monotonically
/// non-decreasing, the `+Inf` bucket must be present and equal the
/// `_count` line, and `_count` must equal the number of `/answer` calls
/// this test actually made — over the *real* router-served body, not a
/// hand-built fixture.
#[tokio::test]
async fn histogram_buckets_are_monotonic_and_count_matches_answer_calls() {
    let state = build_node();
    let app = NodeState::router(state.clone());

    let (status, body, _) = get(&app, "/setup").await;
    assert_eq!(status, StatusCode::OK);
    let bundle = wire::decode_setup(&body).expect("decode_setup");
    let epoch = wire::lineage_epoch(&bundle.backend_params);
    let mut client: RisePirClient<SimplePirBackend> = RisePirClient::from_setup(bundle, codec());

    const N: usize = 5;
    for i in 0..N {
        let key = keccak256(&[i as u8; 20]); // some hit (0x11), some miss — irrelevant to this test
        let (queries, _ctx) = client.build_query(&key);
        let query_bytes = wire::encode_query_bundle(&queries);
        let status = post_answer(&app, &epoch, query_bytes).await;
        assert_eq!(status, StatusCode::OK, "answer #{i}");
    }

    let (status, body, _) = get(&app, "/metrics").await;
    assert_eq!(status, StatusCode::OK);
    let text = String::from_utf8(body).unwrap();
    let samples = parse_and_check(&text);

    let buckets = find(&samples, "risepir_answer_duration_seconds_bucket");
    assert!(!buckets.is_empty(), "no histogram buckets rendered");
    assert!(
        buckets.iter().any(|s| label(s, "le") == Some("+Inf")),
        "the +Inf bucket must always be present"
    );
    let mut prev = 0.0f64;
    for b in &buckets {
        assert!(
            b.value >= prev,
            "bucket counts must be monotonically non-decreasing: {text}"
        );
        prev = b.value;
    }
    let inf_count = buckets
        .iter()
        .find(|s| label(s, "le") == Some("+Inf"))
        .unwrap()
        .value;

    let count_line = find(&samples, "risepir_answer_duration_seconds_count");
    assert_eq!(count_line.len(), 1, "exactly one _count line");
    assert_eq!(
        count_line[0].value, N as f64,
        "the histogram must have observed exactly the {N} /answer calls this test made"
    );
    assert_eq!(
        inf_count, count_line[0].value,
        "the +Inf bucket must equal _count"
    );

    let sum_line = find(&samples, "risepir_answer_duration_seconds_sum");
    assert_eq!(sum_line.len(), 1);
    assert!(
        sum_line[0].value >= 0.0,
        "a duration sum can never be negative"
    );
}

/// `risepir_requests_total{route,outcome}` and
/// `risepir_request_errors_total{route,class}` must reflect real request
/// outcomes: a successful `/answer` bumps `outcome="ok"`; a malformed body
/// (bad magic, decoded by `crate::wire::decode_query_bundle`) bumps
/// `outcome="error"` with `class="BadMagic"` — the WireError variant name,
/// exactly as ADR-0039 requires, never a formatted message.
#[tokio::test]
async fn request_and_error_counters_reflect_real_traffic() {
    let state = build_node();
    let app = NodeState::router(state.clone());
    let epoch = state.epoch().to_string();

    let (status, body, _) = get(&app, "/setup").await;
    assert_eq!(status, StatusCode::OK);
    let bundle = wire::decode_setup(&body).expect("decode_setup");
    let mut client: RisePirClient<SimplePirBackend> = RisePirClient::from_setup(bundle, codec());

    let key = keccak256(&[0x11u8; 20]);
    let (queries, _ctx) = client.build_query(&key);
    let ok_status = post_answer(&app, &epoch, wire::encode_query_bundle(&queries)).await;
    assert_eq!(ok_status, StatusCode::OK);

    // Not "RPQ1" -> WireError::BadMagic, before any query-shaped content is
    // even inspected.
    let bad_status = post_answer(&app, &epoch, b"not a valid query bundle at all".to_vec()).await;
    assert_eq!(bad_status, StatusCode::BAD_REQUEST);

    let (status, body, _) = get(&app, "/metrics").await;
    assert_eq!(status, StatusCode::OK);
    let text = String::from_utf8(body).unwrap();
    let samples = parse_and_check(&text);

    let ok_count = find(&samples, "risepir_requests_total")
        .into_iter()
        .find(|s| label(s, "route") == Some("answer") && label(s, "outcome") == Some("ok"))
        .unwrap_or_else(|| {
            panic!("no risepir_requests_total{{route=answer,outcome=ok}} sample in:\n{text}")
        });
    assert_eq!(ok_count.value, 1.0);

    let err_count = find(&samples, "risepir_requests_total")
        .into_iter()
        .find(|s| label(s, "route") == Some("answer") && label(s, "outcome") == Some("error"))
        .unwrap_or_else(|| {
            panic!("no risepir_requests_total{{route=answer,outcome=error}} sample in:\n{text}")
        });
    assert_eq!(err_count.value, 1.0);

    let class_count = find(&samples, "risepir_request_errors_total")
        .into_iter()
        .find(|s| label(s, "route") == Some("answer") && label(s, "class") == Some("BadMagic"))
        .unwrap_or_else(|| {
            panic!(
                "no risepir_request_errors_total{{route=answer,class=BadMagic}} sample in:\n{text}"
            )
        });
    assert_eq!(class_count.value, 1.0);
}

/// The privacy tripwire (ADR-0039): nothing in the real served body may
/// look address- or ciphertext-segment-shaped — a cheap, blunt regex-style
/// assertion (no run of 40+ hex digits, the length of a 20-byte address in
/// hex) over the response actually served after real `/answer` traffic
/// (including a query for a real seeded account), not merely a hand-built
/// fixture. `crate::metrics`'s own unit test pins the same property at the
/// rendering-function level; this is the same tripwire one layer up, over
/// the router-served bytes.
#[tokio::test]
async fn served_metrics_body_contains_nothing_address_shaped() {
    let state = build_node();
    let app = NodeState::router(state);

    let (status, body, _) = get(&app, "/setup").await;
    assert_eq!(status, StatusCode::OK);
    let bundle = wire::decode_setup(&body).expect("decode_setup");
    let epoch = wire::lineage_epoch(&bundle.backend_params);
    let mut client: RisePirClient<SimplePirBackend> = RisePirClient::from_setup(bundle, codec());

    // Query the one real seeded account by name.
    let key = keccak256(&[0x11u8; 20]);
    let (queries, _ctx) = client.build_query(&key);
    let status = post_answer(&app, &epoch, wire::encode_query_bundle(&queries)).await;
    assert_eq!(status, StatusCode::OK);

    let (status, body, _) = get(&app, "/metrics").await;
    assert_eq!(status, StatusCode::OK);
    let text = String::from_utf8(body).unwrap();

    for token in text.split(|c: char| !c.is_ascii_hexdigit()) {
        assert!(
            token.len() < 40,
            "found a 40+ hex-digit run, address-shaped: {token:?} in the served /metrics body"
        );
    }
}
