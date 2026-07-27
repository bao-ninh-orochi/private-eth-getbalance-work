//! Item 6: automatic re-bootstrap on `Stalled` (`PrivateEth::get_balance`).
//!
//! Before this fix, a rewind client whose `pending_head` fell out of the
//! server's `DeltaRing` retention window wedged **permanently**: every
//! subsequent `eth_getBalance` re-requested the same aged-out range and
//! got the identical `RpcError::Stalled`, forever, with no recovery short
//! of restarting the process. `PrivateEth::get_balance` now re-bootstraps
//! once (a fresh `GET /mode` + `GET /setup`, exactly what a freshly
//! started process does) and retries once on exactly that error.
//!
//! Real-socket integration tests only — a real `NodeState` + axum router
//! on a real ephemeral TCP port, driven by a real `PirHttpClient` and a
//! real `PrivateEth`, mirroring `risepir-http/tests/client.rs`'s harness
//! — using only `risepir_rpc`'s public surface (`PrivateEth::from_setup`,
//! `get_balance`, `pinned_block`). Every test here is deterministic: the
//! "aged out of the ring" condition is produced by making the
//! `DeltaRing`'s capacity smaller than the span being requested — which
//! `DeltaRing::range` reports as `None` unconditionally (see
//! `risepir-server/src/delta_ring.rs`), not by racing against a
//! background writer — and the "even a fresh bootstrap immediately stalls
//! again" case (test 4) is produced by a small deterministic middleware
//! that fakes only `GET /head` / `GET /sync`, never by timing.

use std::sync::Arc;
use std::time::Duration;

use axum::extract::Request;
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};

use ikpir_common::{SimpleConfig, SimplePirBackend};
use risepir_http::{NodeState, PirHttpClient};
use risepir_proto::{keccak256, AddressHash, Backend, BlockUpdate, Geometry, ValueCodec};
use risepir_rpc::{PrivateEth, RpcError};
use risepir_server::{DeltaRing, RisePirServer};
use segmented_cuckoo::{Segmented3aryCuckooKVStore, Segmented3aryScheme};

const ARITY: u32 = 3;
const BUCKET_SIZE: u32 = 4;
const FINGERPRINT_BITS: u32 = 32;
// Small on purpose, for test speed — the same trade-off `demo.rs` and
// every other in-process test suite in this workspace makes; security is
// not the point of these tests, the retry wiring is.
const LWE_DIM: u32 = 512;

/// A deterministic, distinctive chain id (never Ethereum mainnet's `1`),
/// mirroring `tests/rpc.rs`'s own convention.
const TEST_CHAIN_ID: u64 = 1337;

fn codec() -> ValueCodec {
    ValueCodec {
        key_tag_bits: 32,
        balance_bits: 96,
        checksum_bits: 16,
    }
}

/// A distinguishable 20-byte address (mirrors every other test file's
/// `[byte; 20]` convention).
fn addr(byte: u8) -> [u8; 20] {
    [byte; 20]
}

/// Builds a small node — `genesis` accounts pre-inserted at block 0, room
/// for up to `capacity` accounts total, a caller-chosen (deliberately
/// tiny, in these tests) [`DeltaRing`] capacity, and `complete` mode —
/// and serves it on a real ephemeral TCP port. Mirrors
/// `risepir-http/tests/client.rs`'s `spawn_node`, parameterized over the
/// knobs this file's tests actually need to vary. Returns the base URL
/// plus a handle to keep driving blocks against it directly.
async fn spawn_test_node(capacity: u64, ring_capacity: usize, complete: bool, genesis: &[(AddressHash, u128)]) -> (String, Arc<NodeState>) {
    let value_codec = codec();
    let geom = Geometry::for_accounts(capacity, ARITY, BUCKET_SIZE, FINGERPRINT_BITS, &value_codec, Backend::Simple).expect("geometry");
    let mut store = Segmented3aryCuckooKVStore::new(
        geom.num_buckets,
        geom.bucket_size,
        geom.fingerprint_bits,
        geom.value_bits,
        geom.plaintext_bits,
    )
    .expect("store");
    for (key, balance) in genesis {
        let encoded = value_codec.encode(key, *balance).expect("encode genesis balance");
        store.insert(*key, &encoded).expect("insert genesis balance");
    }

    let server: RisePirServer<Segmented3aryScheme, SimplePirBackend> =
        RisePirServer::new(store, SimpleConfig::with_lwe_dim(LWE_DIM), value_codec, 0);
    let state = Arc::new(NodeState::new(server, DeltaRing::new(ring_capacity), complete));
    let router = NodeState::router(state.clone());

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local_addr");
    tokio::spawn(async move {
        axum::serve(listener, router).await.expect("axum::serve");
    });

    (format!("http://{addr}"), state)
}

/// A block update touching a handful of addresses whose top byte is
/// always `0xEE` — a value no fixed test address in this file ever uses
/// (see `addr`, which only ever produces `[byte; 20]` with `byte < 0x80`
/// in this file) — so repeatedly "churning" the chain to advance the head
/// can never collide with a balance a test is actually asserting on.
fn churn_update(block: u64) -> BlockUpdate {
    let mut changes = Vec::with_capacity(3);
    for i in 0..3u64 {
        let mut key: AddressHash = [0u8; 32];
        key[0] = 0xEE;
        key[24..].copy_from_slice(&(block * 8 + i).to_be_bytes());
        changes.push((key, 1u128));
    }
    BlockUpdate { block, changes, credits: Vec::new() }
}

/// Bootstraps a [`PrivateEth`] exactly the way `front.rs`'s `spawn` does:
/// `GET /mode` then `GET /setup`, never guessing either.
async fn bootstrap(base: &str, chain_id: u64) -> PrivateEth {
    let pir = PirHttpClient::new(base);
    let complete = pir.mode().await.expect("GET /mode");
    let bundle = pir.setup().await.expect("GET /setup");
    PrivateEth::from_setup(pir, bundle, codec(), complete, chain_id, None)
}

// ── 1 & 2: the wedge is gone, and the recovered balance is exact ───────

#[tokio::test]
async fn stalled_client_rebootstraps_and_recovers_the_correct_balance() {
    const TARGET_BALANCE: u128 = 123_456_789_000_000_000_000u128;
    let target_addr = addr(0x42);
    let target_key = keccak256(&target_addr);

    // Ring capacity 3, deliberately far smaller than the 20 "unrelated"
    // blocks applied below: `DeltaRing::range` is `None` unconditionally
    // once the requested span exceeds its capacity (`selected.len()` can
    // never reach `want` when capacity < want) — deterministic, not a
    // timing race.
    let (base, state) = spawn_test_node(1_000, 3, true, &[(target_key, TARGET_BALANCE)]).await;

    let private_eth = bootstrap(&base, TEST_CHAIN_ID).await;
    assert_eq!(private_eth.pinned_block().await, 0, "sanity: pinned at genesis");

    // Sanity: the balance is reachable before the ring ages out — proves
    // the eventual success below is really about recovering from the
    // wedge, not a setup bug that would have failed regardless.
    assert_eq!(
        private_eth.get_balance(target_addr).await.expect("pre-wedge lookup must succeed"),
        TARGET_BALANCE
    );

    // Advance the chain, touching only unrelated addresses, far past the
    // ring's tiny capacity.
    for block in 1..=20u64 {
        state.apply_block(&churn_update(block)).await.expect("apply_block");
    }

    // Old behaviour: this call's internal sync of (0, 20] against a
    // 3-block ring 409s, and `Stalled` was returned from here on, forever.
    // Fixed behaviour: get_balance re-bootstraps once (fresh GET /mode +
    // GET /setup, pinned at the current head) and retries once.
    let recovered = private_eth
        .get_balance(target_addr)
        .await
        .expect("get_balance must recover via one automatic re-bootstrap, not return Stalled");
    assert_eq!(recovered, TARGET_BALANCE, "recovered balance must be byte-exact");

    assert_eq!(
        private_eth.pinned_block().await,
        20,
        "the session's pinned block must have advanced to the server's head via the re-bootstrap"
    );

    // Healthy afterward too, not just "recovered once by accident": a
    // subsequent ordinary call (no further stall) still works.
    assert_eq!(private_eth.get_balance(target_addr).await.expect("post-recovery lookup"), TARGET_BALANCE);
}

// ── 3: strict_not_found is re-derived, never carried over stale ────────

#[tokio::test]
async fn rebootstrap_recovers_strict_not_found_after_a_stale_complete_assumption() {
    // A genuinely PARTIAL deployment.
    let (base, state) = spawn_test_node(1_000, 3, /* complete = */ false, &[]).await;

    // Bootstrap `PrivateEth` as if the deployment were COMPLETE — the
    // stale belief `PrivateEth::rebootstrap`'s docs describe (e.g. a
    // client that bootstrapped before an operator restarted the
    // deployment from a complete snapshot down to `--partial`). This
    // uses `PrivateEth::from_setup` directly (not the `bootstrap` helper
    // above, which always fetches the *real* mode) specifically to
    // construct that stale state.
    let pir = PirHttpClient::new(&base);
    let bundle = pir.setup().await.expect("GET /setup");
    let private_eth = PrivateEth::from_setup(pir, bundle, codec(), /* complete = */ true, TEST_CHAIN_ID, None);

    let untracked = addr(0x99);

    // Sanity / non-vacuity: with the wrong (stale) policy in place and
    // nothing yet forcing a re-bootstrap, the bug this whole fix is
    // about reproduces here too — 0x0 for an address this partial
    // deployment has never tracked. Confirms the deliberately-stale
    // bootstrap really did land the wrong policy.
    assert_eq!(
        private_eth
            .get_balance(untracked)
            .await
            .expect("no stall yet; must use the stale (wrong, complete-mode) policy"),
        0,
        "sanity: the deliberately-stale bootstrap must currently answer 0x0 until something \
         re-derives strict_not_found"
    );

    // Force a stall: advance past the tiny ring capacity.
    for block in 1..=20u64 {
        state.apply_block(&churn_update(block)).await.expect("apply_block");
    }

    // The automatic re-bootstrap re-fetches GET /mode fresh, which must
    // correct strict_not_found to match the deployment's REAL (partial)
    // mode — so the very same untracked address must now error rather
    // than silently answer 0x0.
    let result = private_eth.get_balance(untracked).await;
    match result {
        Err(RpcError::NotInTrackedSet) => {}
        other => panic!(
            "after the automatic re-bootstrap, an untracked address in a partial deployment must be \
             RpcError::NotInTrackedSet, never {other:?} — a stale strict_not_found would silently \
             answer 0x0, exactly the bug this fix closes"
        ),
    }
}

// ── 4: bounded retry — a second consecutive stall is reported, never
//      retried forever ────────────────────────────────────────────────

/// Fixed "current head" this test's mock server always reports — chosen
/// far beyond block 0 (what `GET /setup` always returns here, since the
/// underlying node never actually advances in this test) so every
/// `sync_to` call has a nonzero span to request.
const FAR_AHEAD_HEAD: u64 = 999_999;

/// Deterministically fakes only `GET /head` (always [`FAR_AHEAD_HEAD`])
/// and `GET /sync` (always `409`); every other path (`/mode`, `/setup`,
/// `/answer`) passes through to the real `NodeState` handlers untouched.
/// Models a deployment whose head is permanently, unboundedly ahead of
/// whatever a fresh `GET /setup` snapshot reflects — the real-world case
/// this guards (a server replaying a catch-up backlog faster than any
/// client can bootstrap against it) — without depending on any timing
/// race against a background writer.
async fn force_stall(req: Request, next: Next) -> Response {
    match req.uri().path() {
        "/head" => (StatusCode::OK, FAR_AHEAD_HEAD.to_le_bytes().to_vec()).into_response(),
        "/sync" => (StatusCode::CONFLICT, "test: permanently out of window").into_response(),
        _ => next.run(req).await,
    }
}

/// Like [`spawn_test_node`], but wrapped with [`force_stall`] — see that
/// function's docs.
async fn spawn_permanently_stalled_node(capacity: u64, complete: bool) -> String {
    let value_codec = codec();
    let geom = Geometry::for_accounts(capacity, ARITY, BUCKET_SIZE, FINGERPRINT_BITS, &value_codec, Backend::Simple).expect("geometry");
    let store = Segmented3aryCuckooKVStore::new(
        geom.num_buckets,
        geom.bucket_size,
        geom.fingerprint_bits,
        geom.value_bits,
        geom.plaintext_bits,
    )
    .expect("store");

    let server: RisePirServer<Segmented3aryScheme, SimplePirBackend> =
        RisePirServer::new(store, SimpleConfig::with_lwe_dim(LWE_DIM), value_codec, 0);
    // The ring's real capacity is irrelevant here (`force_stall` overrides
    // `/sync` unconditionally); `DeltaRing::new` just requires >= 1.
    let state = Arc::new(NodeState::new(server, DeltaRing::new(1), complete));
    let router = NodeState::router(state).layer(middleware::from_fn(force_stall));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local_addr");
    tokio::spawn(async move {
        axum::serve(listener, router).await.expect("axum::serve");
    });

    format!("http://{addr}")
}

#[tokio::test]
async fn a_second_consecutive_stall_is_reported_not_retried_forever() {
    let base = spawn_permanently_stalled_node(100, true).await;
    let private_eth = bootstrap(&base, TEST_CHAIN_ID).await;
    assert_eq!(private_eth.pinned_block().await, 0, "sanity: pinned at genesis");

    // First attempt: /head always reports 999_999, so sync_to(0, 999_999)
    // calls /sync, which always 409s -> Stalled. get_balance then
    // re-bootstraps once: a real GET /mode + GET /setup, both still
    // pinned at block 0 (the underlying node never advances in this
    // test) -> the retry's own sync_to hits the exact same forced 409.
    // Must surface as Stalled after exactly that one retry — never hang,
    // never loop. The timeout turns a hypothetical regression (an
    // unbounded retry loop) into a fast, clean test failure instead of a
    // hang.
    let result = tokio::time::timeout(Duration::from_secs(10), private_eth.get_balance(addr(0x01)))
        .await
        .expect("get_balance must not hang: a bounded retry, never an unbounded loop against a permanently-stalled server");

    match result {
        Err(RpcError::Stalled) => {}
        other => panic!("a permanently out-of-window server must surface as RpcError::Stalled after exactly one retry, got {other:?}"),
    }

    // And the bounded retry must not have mutated the session into some
    // half-updated state: still pinned at the same block a rebootstrap
    // would always land on here (0 — the underlying node never advances).
    assert_eq!(private_eth.pinned_block().await, 0);
}

/// The re-download meter (ADR-0029, amended): each stalled `get_balance`
/// used to run its *own* full re-bootstrap — at the live complete set,
/// 830.73 MB of `/setup` per call — so a polling caller against a
/// replaying server became an unmetered download loop. Within
/// `REBOOTSTRAP_COOLDOWN` of an attempt, further stalled calls must
/// report the stall without touching `/setup` again.
#[tokio::test]
async fn a_rebootstrap_within_the_cooldown_is_not_paid_again() {
    use axum::extract::{Request as AxRequest, State};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc as StdArc;

    async fn count_setup_and_stall(
        State(counter): State<StdArc<AtomicUsize>>,
        req: AxRequest,
        next: Next,
    ) -> Response {
        match req.uri().path() {
            "/head" => (StatusCode::OK, FAR_AHEAD_HEAD.to_le_bytes().to_vec()).into_response(),
            "/sync" => (StatusCode::CONFLICT, "test: permanently out of window").into_response(),
            "/setup" => {
                counter.fetch_add(1, Ordering::SeqCst);
                next.run(req).await
            }
            _ => next.run(req).await,
        }
    }

    let setup_fetches = StdArc::new(AtomicUsize::new(0));
    let value_codec = codec();
    let geom = Geometry::for_accounts(100, ARITY, BUCKET_SIZE, FINGERPRINT_BITS, &value_codec, Backend::Simple)
        .expect("geometry");
    let store = Segmented3aryCuckooKVStore::new(
        geom.num_buckets,
        geom.bucket_size,
        geom.fingerprint_bits,
        geom.value_bits,
        geom.plaintext_bits,
    )
    .expect("store");
    let server: RisePirServer<Segmented3aryScheme, SimplePirBackend> =
        RisePirServer::new(store, SimpleConfig::with_lwe_dim(LWE_DIM), value_codec, 0);
    let state = Arc::new(NodeState::new(server, DeltaRing::new(1), true));
    let router = NodeState::router(state)
        .layer(middleware::from_fn_with_state(StdArc::clone(&setup_fetches), count_setup_and_stall));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind ephemeral port");
    let sock_addr = listener.local_addr().expect("local_addr");
    tokio::spawn(async move {
        axum::serve(listener, router).await.expect("axum::serve");
    });
    let base = format!("http://{sock_addr}");

    let private_eth = bootstrap(&base, TEST_CHAIN_ID).await;
    assert_eq!(setup_fetches.load(Ordering::SeqCst), 1, "bootstrap = one /setup");

    // First stalled call: consumes the one cooldown slot — exactly one
    // more /setup — and still (this server never un-stalls) reports
    // Stalled.
    match private_eth.get_balance(addr(0x02)).await {
        Err(RpcError::Stalled) => {}
        other => panic!("expected Stalled, got {other:?}"),
    }
    assert_eq!(setup_fetches.load(Ordering::SeqCst), 2, "one re-bootstrap = one more /setup");

    // Immediate follow-up calls, well inside the cooldown: the stall is
    // reported honestly and /setup is NOT fetched again — the meter, not
    // the recovery, is what these calls exercise.
    for i in 0..3u8 {
        match private_eth.get_balance(addr(0x03 + i)).await {
            Err(RpcError::Stalled) => {}
            other => panic!("expected Stalled on cooldown call {i}, got {other:?}"),
        }
    }
    assert_eq!(
        setup_fetches.load(Ordering::SeqCst),
        2,
        "calls within the cooldown must not pay another /setup download"
    );
}
