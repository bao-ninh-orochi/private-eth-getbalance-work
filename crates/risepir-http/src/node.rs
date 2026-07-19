//! axum HTTP server exposing the RisePIR endpoints
//! (`docs/plan.md` §3.4, ADR-0006) over a [`tokio::sync::RwLock`]-guarded
//! [`RisePirServer`] (ADR-0010: the concrete server type is
//! auto-`Send + Sync`, so the lock gives concurrent readers on the hot
//! `/answer` path).

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, RawQuery, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use tokio::sync::RwLock;

use ikpir_common::backend::simple::SimpleServerParams;
use ikpir_common::SimplePirBackend;
use risepir_proto::{codec, BlockDelta, BlockUpdate};
use risepir_server::{DeltaRing, RisePirServer, ServerError};
use segmented_cuckoo::Segmented3aryScheme;

use crate::wire;

/// Maximum accepted `POST /answer` body size, in bytes. Generous but
/// finite — bounds request-buffering memory before any wire decoding even
/// starts (`docs/plan.md`'s "malformed/hostile bytes" hazard applies to
/// the transport layer too, not just the codec: an attacker should not be
/// able to force an unbounded read just by sending a very long body).
pub const MAX_ANSWER_BODY_BYTES: usize = 8 * 1024 * 1024; // 8 MiB

/// Everything the lock guards: the PIR server, its sliding delta-ring
/// window, and a bounded per-block delta index for immutable
/// `GET /delta/{block}` lookups. `per_block` exists because [`DeltaRing`]
/// only answers coalesced *ranges* (`range(from, to)`), not "give me
/// exactly block N's own delta" — ADR-0006 wants deltas served as
/// immutable per-block objects, so this index keeps the individual
/// per-block bytes retrievable, capped to exactly the ring's own retention
/// window (see [`NodeState::apply_block`]).
struct Inner {
    server: RisePirServer<Segmented3aryScheme, SimplePirBackend>,
    ring: DeltaRing,
    per_block: BTreeMap<u64, BlockDelta>,
}

/// Shared server state behind `Arc` (ADR-0010).
pub struct NodeState {
    inner: RwLock<Inner>,
    /// Whether the served set is the *complete* nonzero-balance universe.
    /// Served to clients via `GET /mode` because the `NotFound` policy
    /// hangs on it (complete ⇒ absence is exactly `0x0`, ADR-0015;
    /// partial ⇒ absence is *unknown* and must error) — a remote front
    /// end guessing this flag would be a silent-wrong-answer bug.
    complete: bool,
    /// Per-segment SimplePIR reshape geometry (`reshape_rows` /
    /// `reshape_row_width`), cached once at construction.
    ///
    /// `RisePirServer` exposes no lighter-weight accessor than
    /// [`RisePirServer::setup`], which also clones every segment's *hint*
    /// (potentially tens of MB) — far too expensive to pay on every
    /// `/answer` request just to learn a handful of `u32` reshape
    /// dimensions. Those dimensions never change across
    /// [`RisePirServer::apply_block`] calls (only a `full_rebuild`, which
    /// this server never performs, would change them), so paying the
    /// `.setup()` cost once here and keeping only the cheap
    /// `backend_params` — never the hints — is sound.
    ///
    /// This is what lets [`wire::decode_query_bundle`] /
    /// [`wire::decode_response_bundle`] bound each segment's `Vec<u32>` to
    /// its *exact* expected length instead of merely "however many 4-byte
    /// groups fit the body" — see those functions' docs for the
    /// out-of-bounds server/client panic that exactness closes.
    backend_params: Vec<SimpleServerParams>,
}

impl NodeState {
    /// Wrap a freshly-built [`RisePirServer`] plus an (initially empty)
    /// [`DeltaRing`] for HTTP serving. The per-block delta index starts
    /// empty and grows as [`Self::apply_block`] is called. `complete`
    /// declares whether the served set is the complete nonzero-balance
    /// universe — see the field's docs; a mock deployment's synthetic
    /// universe is complete by construction.
    pub fn new(
        server: RisePirServer<Segmented3aryScheme, SimplePirBackend>,
        ring: DeltaRing,
        complete: bool,
    ) -> Self {
        // One-time cost, paid once at startup — see the field's docs.
        let backend_params = server.setup().backend_params;
        Self {
            inner: RwLock::new(Inner {
                server,
                ring,
                per_block: BTreeMap::new(),
            }),
            backend_params,
            complete,
        }
    }

    /// Apply one block: the sole writer path. Applies to the server,
    /// pushes the resulting delta to the ring, and indexes it by block
    /// number — all under a single write-lock acquisition, so (per
    /// ADR-0010) `apply_block` and any concurrent `/answer` are atomic
    /// with respect to each other. Not exposed over HTTP: the
    /// block-following driver (outside this crate's Stage-0.3 scope) calls
    /// this directly.
    ///
    /// After indexing, evicts every `per_block` entry older than the
    /// ring's own new [`DeltaRing::oldest`] — keeping `per_block`'s
    /// retention exactly in sync with the ring's, with no separate
    /// capacity constant to drift out of agreement.
    ///
    /// # Errors
    ///
    /// Whatever [`RisePirServer::apply_block`] returns — see
    /// [`ServerError`]. On `Err`, nothing is pushed to the ring or
    /// `per_block` (matching that method's own "no partial delta leaks"
    /// guarantee).
    pub async fn apply_block(&self, update: &BlockUpdate) -> Result<(), ServerError> {
        let mut inner = self.inner.write().await;
        let delta = inner.server.apply_block(update)?;
        inner.ring.push(delta.clone());
        inner.per_block.insert(delta.block, delta);
        if let Some(oldest) = inner.ring.oldest() {
            let stale: Vec<u64> = inner.per_block.range(..oldest).map(|(b, _)| *b).collect();
            for b in stale {
                inner.per_block.remove(&b);
            }
        }
        Ok(())
    }

    /// Verified read of the balance currently stored for `key`
    /// ([`RisePirServer::balance_of`]), under the read lock — what the
    /// reconciliation loop diffs against an independent reference RPC
    /// (`docs/sync.md`).
    pub async fn balance_of(
        &self,
        key: &risepir_proto::AddressHash,
    ) -> Result<Option<risepir_proto::Balance>, ServerError> {
        self.inner.read().await.server.balance_of(key)
    }

    /// Run `f` over the wrapped server under the read lock — the escape
    /// hatch state persistence uses to snapshot cells/setup/num_items
    /// atomically with respect to the block-follow writer, without this
    /// crate having to know any state-file format.
    pub async fn with_server<R>(
        &self,
        f: impl FnOnce(&RisePirServer<Segmented3aryScheme, SimplePirBackend>) -> R,
    ) -> R {
        f(&self.inner.read().await.server)
    }

    /// Build the axum [`Router`] exposing every RisePIR HTTP endpoint over
    /// `state`. All routes are read-side (`/answer` takes the state's read
    /// lock, never the write lock) — [`Self::apply_block`] is driven
    /// directly by the caller, not through this router.
    pub fn router(state: Arc<NodeState>) -> Router {
        Router::new()
            .route("/answer", post(answer))
            .route("/delta/{block}", get(delta_by_block))
            .route("/sync", get(sync))
            .route("/setup", get(setup))
            .route("/head", get(head))
            .route("/mode", get(mode))
            .layer(DefaultBodyLimit::max(MAX_ANSWER_BODY_BYTES))
            .with_state(state)
    }
}

// ─── handlers ──────────────────────────────────────────────────────────
//
// Every handler is panic-free on adversarial input: wire decode errors and
// query-processing errors both map to a clean `400 Bad Request` with the
// error's `Display` text, never a 500 or a panic.

/// `POST /answer`: decode the query bundle, answer at the server's current
/// head, encode the response bundle. Read lock only.
async fn answer(State(state): State<Arc<NodeState>>, body: Bytes) -> Response {
    let inner = state.inner.read().await;
    let params = inner.server.params();
    let expected_len_per_seg: Vec<u32> = state.backend_params.iter().map(|sp| sp.reshape_rows).collect();

    let queries = match wire::decode_query_bundle(&body, &params, &expected_len_per_seg) {
        Ok(q) => q,
        Err(e) => return bad_request(e),
    };

    let (responses, head) = match inner.server.answer(&queries) {
        Ok(r) => r,
        Err(e) => return bad_request(e),
    };
    drop(inner);

    octet_response(StatusCode::OK, wire::encode_response_bundle(&responses, head))
}

/// `GET /delta/{block}`: the immutable per-block delta, cacheable forever
/// (ADR-0006), or `404` if this block has aged out of (or never entered)
/// the retention window. Read lock only.
async fn delta_by_block(State(state): State<Arc<NodeState>>, Path(block): Path<u64>) -> Response {
    let inner = state.inner.read().await;
    let plaintext_bits = inner.server.params().plaintext_bits;
    let Some(delta) = inner.per_block.get(&block).cloned() else {
        drop(inner);
        return (StatusCode::NOT_FOUND, "no delta retained for that block").into_response();
    };
    drop(inner);

    let bytes = codec::encode_block_delta(&delta, plaintext_bits);
    let mut resp = octet_response(StatusCode::OK, bytes);
    resp.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("public, max-age=31536000, immutable"));
    resp
}

/// `GET /sync?from=<u64>&to=<u64>`: the coalesced delta for `(from, to]`,
/// or `409 Conflict` if any part of that range has aged out of the
/// retention window (the client must resync from `/setup`). Unparseable
/// query params are rejected with `400`, never treated as `0`/silently
/// ignored. Read lock only.
async fn sync(State(state): State<Arc<NodeState>>, RawQuery(raw): RawQuery) -> Response {
    let Some((from, to)) = raw.as_deref().and_then(parse_sync_query) else {
        return (StatusCode::BAD_REQUEST, "expected query params ?from=<u64>&to=<u64>").into_response();
    };

    let inner = state.inner.read().await;
    let plaintext_bits = inner.server.params().plaintext_bits;
    let delta = inner.ring.range(from, to);
    drop(inner);

    match delta {
        Some(delta) => octet_response(StatusCode::OK, codec::encode_block_delta(&delta, plaintext_bits)),
        None => (
            StatusCode::CONFLICT,
            "requested range is outside the retained delta window; a full resync via /setup is required",
        )
            .into_response(),
    }
}

/// `GET /setup`: the full [`risepir_server::SetupBundle`] a fresh client
/// bootstraps from. Read lock only.
async fn setup(State(state): State<Arc<NodeState>>) -> Response {
    let inner = state.inner.read().await;
    let bundle = inner.server.setup();
    drop(inner);
    octet_response(StatusCode::OK, wire::encode_setup(&bundle))
}

/// `GET /head`: the server's current block, as an 8-byte little-endian
/// `u64` body. Read lock only.
async fn head(State(state): State<Arc<NodeState>>) -> Response {
    let inner = state.inner.read().await;
    let h = inner.server.block();
    drop(inner);
    octet_response(StatusCode::OK, h.to_le_bytes().to_vec())
}

/// `GET /mode`: one byte — `1` if this deployment serves the *complete*
/// nonzero-balance set, `0` if partial. A remote front end keys its
/// `NotFound` policy on this (never guessed; see [`NodeState::new`]).
/// No lock needed: the flag is fixed at construction.
async fn mode(State(state): State<Arc<NodeState>>) -> Response {
    octet_response(StatusCode::OK, vec![u8::from(state.complete)])
}

// ─── small helpers ────────────────────────────────────────────────────

/// Builds a `200`-or-caller-chosen-`status` response with an explicit
/// `application/octet-stream` content type — every successful response
/// this crate returns is raw binary, never text or JSON.
fn octet_response(status: StatusCode, body: Vec<u8>) -> Response {
    let mut resp = (status, body).into_response();
    resp.headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static("application/octet-stream"));
    resp
}

/// `400 Bad Request` with `err`'s `Display` text as the body — the uniform
/// mapping every decode/processing error in this crate's handlers goes
/// through, so a caller always gets a legible reason rather than an opaque
/// status code.
fn bad_request(err: impl std::fmt::Display) -> Response {
    (StatusCode::BAD_REQUEST, err.to_string()).into_response()
}

/// Parses `from=<u64>&to=<u64>` (order-independent, extra/unknown params
/// ignored) out of a raw query string. `None` for anything that doesn't
/// cleanly parse both — the caller maps that to `400`, never guessing a
/// default.
fn parse_sync_query(q: &str) -> Option<(u64, u64)> {
    let mut from: Option<u64> = None;
    let mut to: Option<u64> = None;
    for pair in q.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (key, val) = pair.split_once('=')?;
        match key {
            "from" => from = val.parse::<u64>().ok(),
            "to" => to = val.parse::<u64>().ok(),
            _ => {}
        }
    }
    match (from, to) {
        (Some(f), Some(t)) => Some((f, t)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_sync_query_accepts_both_orders() {
        assert_eq!(parse_sync_query("from=1&to=5"), Some((1, 5)));
        assert_eq!(parse_sync_query("to=5&from=1"), Some((1, 5)));
    }

    #[test]
    fn parse_sync_query_ignores_unknown_params() {
        assert_eq!(parse_sync_query("foo=bar&from=1&to=5&baz=qux"), Some((1, 5)));
    }

    #[test]
    fn parse_sync_query_rejects_missing_or_malformed() {
        assert_eq!(parse_sync_query(""), None);
        assert_eq!(parse_sync_query("from=1"), None);
        assert_eq!(parse_sync_query("to=5"), None);
        assert_eq!(parse_sync_query("from=abc&to=5"), None);
        assert_eq!(parse_sync_query("from=1&to=-5"), None);
        assert_eq!(parse_sync_query("garbage"), None);
    }
}
