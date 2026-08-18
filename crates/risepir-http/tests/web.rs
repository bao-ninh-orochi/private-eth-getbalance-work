//! The two server-side pieces the browser front end added (ADR-0019):
//! `GET /recent` and the static-asset routes. Driven in-process via
//! `tower::ServiceExt::oneshot`, like `tests/http.rs` — no real socket.
//!
//! What is being pinned here is not "the page renders" (that is
//! `web/test/browser.mjs`'s job) but the properties that would be
//! *silently* wrong: that the asset routes are a fixed list rather than a
//! path mapping, that the hardening headers are actually sent, and that
//! `/recent` cannot be influenced by a client.

use std::sync::Arc;

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use ikpir_common::{SimpleConfig, SimplePirBackend};
use risepir_http::{NodeState, WebAssets};
use risepir_proto::{Backend, Geometry, ValueCodec};
use risepir_server::{DeltaRing, RisePirServer};
use segmented_cuckoo::{Segmented2aryCuckooKVStore, Segmented2aryScheme};

const ARITY: u32 = 2;
const BUCKET_SIZE: u32 = 4;
const FINGERPRINT_BITS: u32 = 32;
const LWE_DIM: u32 = 256;

fn value_codec() -> ValueCodec {
    ValueCodec {
        key_tag_bits: 32,
        balance_bits: 96,
        checksum_bits: 16,
    }
}

fn build_node() -> Arc<NodeState> {
    let codec = value_codec();
    let geom = Geometry::for_accounts(
        1_000,
        ARITY,
        BUCKET_SIZE,
        FINGERPRINT_BITS,
        &codec,
        Backend::Simple,
    )
    .expect("geometry");
    let store = Segmented2aryCuckooKVStore::new(
        geom.num_buckets,
        geom.bucket_size,
        geom.fingerprint_bits,
        geom.value_bits,
        geom.plaintext_bits,
    )
    .expect("store");
    let server: RisePirServer<Segmented2aryScheme, SimplePirBackend> =
        RisePirServer::new(store, SimpleConfig::with_lwe_dim(LWE_DIM), codec, 0);
    Arc::new(NodeState::new(server, DeltaRing::new(64), true))
}

async fn get(app: &axum::Router, uri: &str) -> (StatusCode, Vec<u8>, axum::http::HeaderMap) {
    let resp = app
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .expect("oneshot");
    let status = resp.status();
    let headers = resp.headers().clone();
    let body = to_bytes(resp.into_body(), usize::MAX)
        .await
        .expect("body")
        .to_vec();
    (status, body, headers)
}

/// The repo's own `web/` directory — the assets a real deployment serves.
/// `client.wasm` is a build artifact, so these tests skip themselves
/// rather than fail when it has not been built (`cargo run -p xtask
/// --release -- web`); the browser gates are what enforce it in anger.
fn web_dir() -> Option<std::path::PathBuf> {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../web");
    dir.join("client.wasm").exists().then_some(dir)
}

fn addr(n: u8) -> [u8; 20] {
    [n; 20]
}

// ─── GET /recent ───────────────────────────────────────────────────────

#[tokio::test]
async fn recent_is_empty_until_the_follower_records_anything() {
    let app = NodeState::router(build_node());
    let (status, body, _) = get(&app, "/recent").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body,
        vec![0, 0, 0, 0],
        "an empty deployment reports a count of zero, not an error"
    );
}

#[tokio::test]
async fn recent_serves_newest_first_and_deduplicates() {
    let state = build_node();
    state.note_recent([addr(1), addr(2), addr(3)]).await;
    // Address 1 seen again: it must move to the front rather than appear
    // twice, so a single busy account cannot crowd the list out.
    state.note_recent([addr(1)]).await;

    let app = NodeState::router(state);
    let (status, body, headers) = get(&app, "/recent").await;
    assert_eq!(status, StatusCode::OK);

    let count = u32::from_le_bytes(body[..4].try_into().unwrap()) as usize;
    assert_eq!(count, 3, "duplicate address was not collapsed");
    assert_eq!(body.len(), 4 + count * 20);
    assert_eq!(
        &body[4..24],
        &addr(1),
        "most recently seen address must come first"
    );
    assert_eq!(&body[24..44], &addr(3));
    assert_eq!(&body[44..64], &addr(2));

    assert_eq!(
        headers
            .get(axum::http::header::CACHE_CONTROL)
            .map(|v| v.to_str().unwrap()),
        Some("no-store"),
        "the suggestion list tracks the chain; a cached copy would go stale silently"
    );
}

#[tokio::test]
async fn recent_is_capped() {
    let state = build_node();
    for i in 0..=255u8 {
        state.note_recent([addr(i)]).await;
    }
    let app = NodeState::router(state);
    let (_, body, _) = get(&app, "/recent").await;
    let count = u32::from_le_bytes(body[..4].try_into().unwrap()) as usize;
    assert_eq!(
        count,
        risepir_http::node::RECENT_CAPACITY,
        "the ring must be bounded"
    );
    assert_eq!(&body[4..24], &addr(255), "and must keep the newest");
}

// ─── static assets ─────────────────────────────────────────────────────

#[test]
fn loading_assets_from_a_directory_without_them_is_a_startup_error() {
    let missing = WebAssets::load(std::path::Path::new("/nonexistent-risepir-web-dir"));
    let msg = match missing {
        Ok(_) => panic!("a missing asset directory must not load"),
        Err(e) => e.to_string(),
    };
    assert!(
        msg.contains("index.html"),
        "the error should name the file it could not read: {msg}"
    );
}

#[test]
fn a_directory_missing_only_the_wasm_says_how_to_build_it() {
    let dir = tempdir();
    for f in ["index.html", "app.js", "pir.js", "style.css"] {
        std::fs::write(dir.join(f), b"x").unwrap();
    }
    let msg = match WebAssets::load(&dir) {
        Ok(_) => panic!("missing client.wasm must not load"),
        Err(e) => e.to_string(),
    };
    assert!(msg.contains("client.wasm"), "{msg}");
    assert!(
        msg.contains("xtask"),
        "the error should say how to produce it: {msg}"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn assets_are_served_with_their_hardening_headers() {
    let Some(dir) = web_dir() else {
        eprintln!("skipping: web/client.wasm not built (cargo run -p xtask --release -- web)");
        return;
    };
    let assets = WebAssets::load(&dir).expect("load web assets");
    assert!(assets.total_bytes() > 1000);
    let app = NodeState::router_with_web(build_node(), Some(assets));

    for (route, content_type) in [
        ("/", "text/html; charset=utf-8"),
        ("/app.js", "text/javascript; charset=utf-8"),
        ("/pir.js", "text/javascript; charset=utf-8"),
        ("/style.css", "text/css; charset=utf-8"),
        ("/client.wasm", "application/wasm"),
        // GET /status (ADR-0039) — the operator page, same mechanism.
        ("/status", "text/html; charset=utf-8"),
        ("/status.css", "text/css; charset=utf-8"),
        ("/status.js", "text/javascript; charset=utf-8"),
    ] {
        let (status, body, headers) = get(&app, route).await;
        assert_eq!(status, StatusCode::OK, "{route}");
        assert!(!body.is_empty(), "{route} served an empty body");
        assert_eq!(
            headers
                .get(axum::http::header::CONTENT_TYPE)
                .unwrap()
                .to_str()
                .unwrap(),
            content_type,
            "{route}"
        );

        let csp = headers
            .get(axum::http::header::CONTENT_SECURITY_POLICY)
            .expect("every asset must carry a CSP")
            .to_str()
            .unwrap();
        // The directive that makes the delivered page structurally unable
        // to send the user's address anywhere but this origin.
        assert!(csp.contains("connect-src 'self'"), "{route}: {csp}");
        // ...and the one without which the client cannot start at all;
        // Chromium treats wasm compilation as script evaluation.
        assert!(csp.contains("'wasm-unsafe-eval'"), "{route}: {csp}");
        assert!(csp.contains("default-src 'none'"), "{route}: {csp}");
        assert_eq!(
            headers
                .get("x-content-type-options")
                .unwrap()
                .to_str()
                .unwrap(),
            "nosniff",
            "{route}"
        );
    }
}

/// There is no request-path-to-filesystem-path mapping to traverse — the
/// routes are a fixed list — so these are all plain 404s. Asserted anyway,
/// because "we serve files from a directory" is the shape of code that
/// grows a traversal bug later.
#[tokio::test]
async fn nothing_outside_the_manifest_is_reachable() {
    let Some(dir) = web_dir() else {
        eprintln!("skipping: web/client.wasm not built");
        return;
    };
    let app = NodeState::router_with_web(build_node(), Some(WebAssets::load(&dir).expect("load")));

    for uri in [
        "/../Cargo.toml",
        "/%2e%2e/Cargo.toml",
        "/test/e2e.mjs",
        "/index.html",
        "/client.wasm/../pir.js",
        "/.git/config",
        "/style.css/",
    ] {
        let (status, _, _) = get(&app, uri).await;
        assert_ne!(status, StatusCode::OK, "{uri} was served");
    }
}

/// Serving the front end must not disturb the PIR API sharing the origin.
#[tokio::test]
async fn the_pir_endpoints_still_answer_alongside_the_assets() {
    let Some(dir) = web_dir() else {
        eprintln!("skipping: web/client.wasm not built");
        return;
    };
    let app = NodeState::router_with_web(build_node(), Some(WebAssets::load(&dir).expect("load")));

    let (status, body, _) = get(&app, "/mode").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, vec![1]);

    let (status, body, _) = get(&app, "/head").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.len(), 8);
}

/// Every front-end route must be counted by `risepir_requests_total`
/// (ADR-0039) — the property `tests/metrics.rs` structurally cannot
/// see, because it builds its router with [`NodeState::router`] (no web
/// assets) and so never exercises this seam at all.
///
/// The bug this pins is an ordering one, and it is silent: `Router::layer`
/// wraps only the routes registered before it is called, so attaching the
/// static assets *after* the metrics middleware left all eight of them
/// serving 200s while incrementing nothing. `route_label`'s `"index"`,
/// `"asset"` and `"status"` arms became unreachable code, and `/status`'s
/// own request table reported zero page loads for the page being read.
/// Nothing about that is visible from a response — only from the counter
/// that failed to move — which is why it survived until it was measured
/// against the live deployment.
#[tokio::test]
async fn every_front_end_route_is_counted_in_the_request_metrics() {
    let Some(dir) = web_dir() else {
        eprintln!("skipping: web/client.wasm not built (cargo run -p xtask --release -- web)");
        return;
    };
    let app = NodeState::router_with_web(build_node(), Some(WebAssets::load(&dir).expect("load")));

    // Every route in `web::MANIFEST`, with the label `route_label` owes it.
    let routes = [
        ("/", "index"),
        ("/app.js", "asset"),
        ("/pir.js", "asset"),
        ("/style.css", "asset"),
        ("/client.wasm", "asset"),
        ("/status", "status"),
        ("/status.css", "asset"),
        ("/status.js", "asset"),
    ];
    for (route, _) in routes {
        let (status, _, _) = get(&app, route).await;
        assert_eq!(status, StatusCode::OK, "{route}");
    }

    let (status, body, _) = get(&app, "/metrics").await;
    assert_eq!(status, StatusCode::OK);
    let text = String::from_utf8(body).expect("metrics is UTF-8 text");

    // `asset` covers five of the eight routes, so assert per-label totals
    // rather than one sample per route: index 1, status 1, asset 5.
    let mut expected: std::collections::BTreeMap<&str, u64> = std::collections::BTreeMap::new();
    for (_, label) in routes {
        *expected.entry(label).or_default() += 1;
    }
    for (label, count) in expected {
        let needle = format!("risepir_requests_total{{route=\"{label}\",outcome=\"ok\"}} {count}");
        assert!(
            text.lines().any(|l| l.trim() == needle),
            "expected `{needle}` in the served exposition — a front-end route served a 200 without \
             being counted, which is exactly the layer-ordering regression this test exists for.\n{text}"
        );
    }

    // And the negative half: a served asset must never fall through to the
    // `unmatched` bucket, which is reserved for genuinely unknown paths.
    assert!(
        !text.contains("route=\"unmatched\""),
        "no request in this test used an unknown path, so `unmatched` must not appear:\n{text}"
    );
}

fn tempdir() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "risepir-web-test-{}-{:?}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}
