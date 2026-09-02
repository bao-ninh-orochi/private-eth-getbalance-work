//! [`PirHttpClient`] — an async HTTP client for the transport
//! [`crate::node`] serves (`docs/plan.md` §3.4, ADR-0006), built on
//! [`reqwest`].
//!
//! # Purpose
//!
//! [`crate::node`] and [`crate::wire`] give a *server* (axum handlers) and
//! a *codec* for this transport; nothing in this crate previously drove it
//! from the *client* side over real HTTP (the existing test suite talks to
//! the router in-process via `tower::oneshot`). Stage 0.4's JSON-RPC front
//! end (`risepir-rpc`) needs exactly that: a `risepir_client::RisePirClient`
//! has no network code of its own (by design — see that crate's docs), so
//! something has to fetch `/setup`, poll `/head`, pull `/sync` deltas, and
//! POST `/answer` bundles over the wire. This module is that something.
//!
//! # Division of labour
//!
//! `PirHttpClient` only ever moves bytes: it encodes/decodes via
//! [`crate::wire`] and [`risepir_proto::codec`] (the same codecs
//! [`crate::node`]'s handlers use) and reports transport/decode failures
//! as a clean [`ClientError`] — it never constructs a
//! `risepir_client::RisePirClient` or interprets a response itself. That
//! keeps the PIR *protocol* logic in exactly one place
//! (`risepir-client`) regardless of whether it is driven in-process or
//! over a real socket.

use std::sync::Arc;
use std::time::{Duration, Instant};

use ikpir_common::backend::simple::{SimpleQuery, SimpleResponse};
use ikpir_common::SimplePirBackend;
use risepir_proto::{codec, BlockDelta};
use risepir_server::SetupBundle;

use crate::measure::{NetCall, NetSink};
use crate::wire::{self, WireError};

// ─── transport guardrails ────────────────────────────────────────────────
//
// This client talks to a server the threat model does *not* ask it to
// trust for availability or resource-safety (§4.2: an on-path attacker or
// dishonest operator controls every byte). Two consequences:
//
// 1. Timeouts. A server that accepts the connection and then stalls must
//    not hang the front end forever. `READ_STALL_TIMEOUT` bounds silence
//    on the socket, not total transfer time — a complete-set `/setup` is
//    gigabytes and legitimately takes minutes; a stalled byte stream is
//    the failure signal, elapsed time is not.
//
// 2. Response-size caps, per endpoint. "Validate every length before
//    allocating" must start at the transport: the decoders bound every
//    parsed length, but a body is buffered *before* decoding, so an
//    unbounded read would let a hostile server OOM the client with a
//    10-terabyte stream. Every cap below is orders of magnitude above any
//    legitimate response at that endpoint.

/// TCP connect bound.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Max silence between bytes on an in-flight response (never total time).
const READ_STALL_TIMEOUT: Duration = Duration::from_secs(30);

/// `/head` (8 bytes) and `/mode` (1 byte).
const MAX_TINY_BODY_BYTES: usize = 64;
/// `/answer`: a response bundle is `arity × reshape_row_width × 4` bytes
/// plus framing — well under a MB at every measured geometry.
const MAX_ANSWER_BODY_BYTES: usize = 64 << 20; // 64 MiB
/// `/sync`: a coalesced delta over the server's whole retention window is
/// a few MB at mainnet change rates (deltas telescope, ADR-0005).
const MAX_SYNC_BODY_BYTES: usize = 1 << 30; // 1 GiB
/// `/setup`: the one legitimately huge response — the full per-segment
/// hint set computes to 553.82 MB at the complete mainnet set's deployed
/// `(arity 2, bucket_size 4)` geometry (ADR-0034; was 830.73 MB at the
/// `(arity 3, bucket_size 4)` geometry measured 2026-07-26, `docs/numbers.md`
/// §4c), so the cap only excludes the absurd.
const MAX_SETUP_BODY_BYTES: usize = 8 << 30; // 8 GiB
/// `/recent`: `count:u32-le ‖ count × 20 bytes`, capped server-side at
/// `node::RECENT_CAPACITY` (128) — a few KiB even with generous slack.
const MAX_RECENT_BODY_BYTES: usize = 1 << 16; // 64 KiB
/// Error bodies are diagnostic text; anything longer is noise.
const MAX_ERROR_BODY_BYTES: usize = 64 << 10; // 64 KiB

/// Optional `POST /answer` response header: nanoseconds the server spent
/// in `RisePirServer::answer` itself.
///
/// **Optional by construction.** A server that does not publish
/// per-answer timings omits it, and every caller here treats its absence
/// as "not reported", never as zero.
const HDR_ANSWER_COMPUTE_NS: &str = "x-risepir-answer-compute-ns";
/// Optional `POST /answer` response header: nanoseconds the server spent
/// in the whole `/answer` handler (lock acquisition, wire decode, the
/// answer, wire encode). Same optionality as [`HDR_ANSWER_COMPUTE_NS`].
const HDR_ANSWER_HANDLER_NS: &str = "x-risepir-answer-handler-ns";

/// Errors from every [`PirHttpClient`] call.
///
/// Deliberately three variants — network / status / wire — per this
/// crate's own "never a panic, always a legible `Err`" discipline
/// ([`crate::wire`]'s module docs): a caller (`risepir-rpc`) can always
/// turn one of these into a JSON-RPC error object without guessing at
/// what went wrong.
#[derive(Debug)]
pub enum ClientError {
    /// The HTTP request itself failed before a response was received at
    /// all — DNS, connection refused, a timeout, or any other
    /// transport-level [`reqwest::Error`].
    Network(reqwest::Error),
    /// The server answered with an HTTP status this call did not expect.
    /// Every method here expects exactly `200 OK` (`/sync`'s `409
    /// Conflict` is handled separately and mapped to `Ok(None)`, never to
    /// this variant — see [`PirHttpClient::sync`]).
    Status {
        /// The status code actually returned.
        status: u16,
        /// The response body, decoded as UTF-8 on a best-effort basis
        /// (empty if the body itself could not be read).
        body: String,
    },
    /// The response body did not decode as the wire format this call
    /// expected: a wrapped [`WireError`] / [`risepir_proto::CodecError`]
    /// (via their `Display`), or — for `/head`, which carries no `wire`
    /// framing of its own, just a raw 8-byte body — a plain description
    /// of why the body was malformed.
    Wire(String),
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Network(e) => write!(f, "network error: {e}"),
            Self::Status { status, body } => write!(f, "unexpected HTTP status {status}: {body}"),
            Self::Wire(msg) => write!(f, "wire decode error: {msg}"),
        }
    }
}

impl std::error::Error for ClientError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Network(e) => Some(e),
            _ => None,
        }
    }
}

impl From<reqwest::Error> for ClientError {
    fn from(e: reqwest::Error) -> Self {
        Self::Network(e)
    }
}

impl From<WireError> for ClientError {
    fn from(e: WireError) -> Self {
        Self::Wire(e.to_string())
    }
}

impl From<risepir_proto::CodecError> for ClientError {
    fn from(e: risepir_proto::CodecError) -> Self {
        Self::Wire(e.to_string())
    }
}

impl ClientError {
    /// This variant's name, and *only* its name — never a formatted
    /// message (see [`crate::wire::WireError::metric_class`]'s identical
    /// rationale: a message here can embed a status body or a decode
    /// diagnosis, either of which may carry lengths/offsets the server
    /// chose; the variant name is a closed, fixed set this crate defines).
    ///
    /// Named here for taxonomy completeness (this crate's `/metrics`,
    /// ADR-0039, names `WireError`/`ServerError`/`ClientError` together as
    /// "the existing error taxonomies"), but note what it is *not* wired
    /// to: `risepir_http::node`'s `/metrics` lives on the PIR server's own
    /// router, and a `ClientError` is never produced by that router — it
    /// is what [`crate::PirHttpClient`] returns to a caller acting as a
    /// client (`risepir-rpc`'s JSON-RPC front end, calling this deployment's
    /// own PIR transport over loopback). Instrumenting *that* call path is
    /// a JSON-RPC-side (`:8545`) concern, out of scope for the PIR-port
    /// `/metrics` this change adds; this method is here so a future caller
    /// that does instrument it does not have to invent the mapping.
    pub fn metric_class(&self) -> &'static str {
        match self {
            Self::Network(_) => "Network",
            Self::Status { .. } => "Status",
            Self::Wire(_) => "Wire",
        }
    }
}

/// Async HTTP client for the RisePIR endpoints [`crate::node::NodeState::router`]
/// exposes: `GET /setup`, `GET /head`, `GET /sync`, `POST /answer`.
///
/// Holds nothing PIR-specific (no [`risepir_proto::geometry::Geometry`],
/// no arity) — every method that needs deployment geometry to decode its
/// response ([`Self::sync`], [`Self::answer`]) takes it as an explicit
/// argument, mirroring [`wire::decode_query_bundle`] /
/// [`wire::decode_response_bundle`]'s own "the caller already holds this
/// from its own `SetupBundle`" contract. This keeps `PirHttpClient` a pure
/// transport concern, reusable unchanged if a future backend or geometry
/// changes what a caller does with the bytes.
pub struct PirHttpClient {
    base: String,
    http: reqwest::Client,
    /// Optional wire-level instrumentation ([`crate::measure`]). `None`
    /// — the default, and what every product construction site uses —
    /// means every method below is byte-for-byte the uninstrumented
    /// path, with no clock read and no lock.
    sink: Option<Arc<NetSink>>,
}

/// One measured round trip: the body (absent only for an allowed `409`)
/// and the response headers, kept so a caller can read `/setup`'s
/// `x-risepir-mode` or `/answer`'s optional timing headers.
struct Measured {
    /// `None` only when the call allowed `409 Conflict` and got one.
    body: Option<Vec<u8>>,
    /// The response's headers, cloned before the body was consumed.
    headers: reqwest::header::HeaderMap,
}

impl PirHttpClient {
    /// Build a client against `base` (e.g. `"http://127.0.0.1:8645"`,
    /// no trailing slash required — one is stripped if present).
    pub fn new(base: impl Into<String>) -> Self {
        let http = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .read_timeout(READ_STALL_TIMEOUT)
            .build()
            // Only fails on TLS-backend initialisation; with the crate's
            // static rustls configuration that is a build-environment
            // defect, not a runtime input.
            .expect("reqwest client construction");
        Self::with_http_client(base, http)
    }

    /// [`Self::new`], but driving a caller-supplied [`reqwest::Client`].
    ///
    /// The one reason this exists: a caller that needs builder options
    /// this crate does not (and should not) expose — a curl-style DNS
    /// override via [`reqwest::ClientBuilder::resolve`], a proxy, a
    /// different timeout budget — while still reusing **one** connection
    /// pool for the whole run. A fresh TCP+TLS handshake per query would
    /// contaminate any measurement of network time, so sharing the
    /// client is not merely convenient.
    ///
    /// The caller owns the configuration, including the guardrails
    /// [`Self::new`] sets ([`CONNECT_TIMEOUT`], [`READ_STALL_TIMEOUT`]):
    /// a client built without them has no timeout at all. The
    /// per-endpoint response-size caps are enforced here regardless, so
    /// the "validate every length before allocating" half of the
    /// transport guardrails cannot be configured away.
    pub fn with_http_client(base: impl Into<String>, http: reqwest::Client) -> Self {
        let base = base.into();
        let base = base.strip_suffix('/').map(str::to_string).unwrap_or(base);
        Self {
            base,
            http,
            sink: None,
        }
    }

    /// Attach a [`NetSink`], so every subsequent call records its wire
    /// time and byte counts (see [`crate::measure`]).
    #[must_use]
    pub fn with_net_sink(mut self, sink: Arc<NetSink>) -> Self {
        self.sink = Some(sink);
        self
    }

    /// The base URL this client talks to, with any trailing slash
    /// already stripped.
    pub fn base(&self) -> &str {
        &self.base
    }

    /// Send one request, measuring it if a [`NetSink`] is attached.
    ///
    /// The measured span starts immediately before `send()` and ends
    /// once the last body byte has been accumulated — so it is wire
    /// time only, with the caller's own decode timed separately via
    /// [`NetSink::record_decode`]. Failures are recorded too, since a
    /// request that never returned still consumed wall time.
    async fn send_measured(
        &self,
        call: NetCall,
        req: reqwest::RequestBuilder,
        request_bytes: u64,
        max_len: usize,
        allow_conflict: bool,
    ) -> Result<Measured, ClientError> {
        let started = self.sink.as_ref().map(|_| Instant::now());
        let record = |response_bytes: u64, content_length: Option<u64>| {
            if let (Some(sink), Some(t0)) = (self.sink.as_ref(), started) {
                sink.record(
                    call,
                    elapsed_ns(t0),
                    request_bytes,
                    response_bytes,
                    content_length,
                );
            }
        };

        let resp = match req.send().await {
            Ok(r) => r,
            Err(e) => {
                record(0, None);
                return Err(e.into());
            }
        };
        let content_length = resp.content_length();
        let headers = resp.headers().clone();
        if allow_conflict && resp.status() == reqwest::StatusCode::CONFLICT {
            record(0, content_length);
            return Ok(Measured {
                body: None,
                headers,
            });
        }
        match ok_body(resp, max_len).await {
            Ok(bytes) => {
                record(bytes.len() as u64, content_length);
                Ok(Measured {
                    body: Some(bytes),
                    headers,
                })
            }
            Err(e) => {
                record(0, content_length);
                Err(e)
            }
        }
    }

    /// Charge `f`'s wall time to `call`'s decode total (a no-op without
    /// a sink), returning whatever `f` returned.
    fn timed_decode<T>(&self, call: NetCall, f: impl FnOnce() -> T) -> T {
        match self.sink.as_ref() {
            None => f(),
            Some(sink) => {
                let t0 = Instant::now();
                let out = f();
                sink.record_decode(call, elapsed_ns(t0));
                out
            }
        }
    }

    /// `GET /setup`: the full [`SetupBundle`] a fresh
    /// `risepir_client::RisePirClient` bootstraps from.
    ///
    /// # Errors
    ///
    /// [`ClientError::Network`] / [`ClientError::Status`] (any status
    /// other than `200`) / [`ClientError::Wire`] (a malformed body — see
    /// [`wire::decode_setup`]).
    pub async fn setup(&self) -> Result<SetupBundle<SimplePirBackend>, ClientError> {
        Ok(self.setup_with_mode().await?.0)
    }

    /// [`Self::setup`], plus the deployment mode read from the *same*
    /// response's `x-risepir-mode` header (`Some(true)` complete /
    /// `Some(false)` partial / `None` when the server predates the header,
    /// ADR-0033).
    ///
    /// Taking the mode from the setup response itself is what makes the
    /// `(bundle, mode)` pair atomic: a separate `GET /mode` can race a
    /// server restart and pair one deployment's completeness policy with
    /// another's data — the mixed state that turns `NotFound` into a
    /// silently wrong `0x0`. Callers should use this header when present
    /// and fall back to [`Self::mode`] only when it is absent.
    ///
    /// # Errors
    ///
    /// As [`Self::setup`]. A malformed (non-`0`/`1`) `x-risepir-mode`
    /// value is [`ClientError::Wire`] — the completeness flag decides the
    /// `NotFound` policy, so a garbled one must never be defaulted or
    /// ignored (`docs/plan.md`'s "never guessed" invariant).
    pub async fn setup_with_mode(
        &self,
    ) -> Result<(SetupBundle<SimplePirBackend>, Option<bool>), ClientError> {
        let m = self
            .send_measured(
                NetCall::Setup,
                self.http.get(format!("{}/setup", self.base)),
                0,
                MAX_SETUP_BODY_BYTES,
                false,
            )
            .await?;
        // Read only after `send_measured` vouched for a `200` — a
        // non-`200` must surface as its status error, not as a header
        // complaint.
        let mode = match m.headers.get("x-risepir-mode").map(|v| v.as_bytes()) {
            None => None,
            Some(b"0") => Some(false),
            Some(b"1") => Some(true),
            Some(other) => {
                return Err(ClientError::Wire(format!(
                    "x-risepir-mode header was {:?} (expected \"0\" or \"1\")",
                    String::from_utf8_lossy(other)
                )))
            }
        };
        let bytes = m.body.unwrap_or_default();
        let bundle = self.timed_decode(NetCall::Setup, || wire::decode_setup(&bytes))?;
        Ok((bundle, mode))
    }

    /// `GET /head`: the server's current block.
    ///
    /// # Errors
    ///
    /// [`ClientError::Network`] / [`ClientError::Status`] / a
    /// [`ClientError::Wire`] if the body is not exactly 8 bytes (the
    /// server's documented `/head` framing — [`crate::node`]'s module
    /// docs).
    pub async fn head(&self) -> Result<u64, ClientError> {
        let m = self
            .send_measured(
                NetCall::Head,
                self.http.get(format!("{}/head", self.base)),
                0,
                MAX_TINY_BODY_BYTES,
                false,
            )
            .await?;
        let bytes = m.body.unwrap_or_default();
        let arr: [u8; 8] = bytes.as_slice().try_into().map_err(|_| {
            ClientError::Wire(format!(
                "/head returned {} bytes, expected exactly 8",
                bytes.len()
            ))
        })?;
        Ok(u64::from_le_bytes(arr))
    }

    /// `GET /mode`: whether the deployment serves the *complete*
    /// nonzero-balance set (`true`) or a partial one (`false`). A remote
    /// front end keys its `NotFound` policy on this — complete ⇒ absence
    /// is exactly `0x0` (ADR-0015), partial ⇒ absence must error — and
    /// must never guess it (`docs/plan.md`'s invariant).
    ///
    /// # Errors
    ///
    /// [`ClientError::Network`] / [`ClientError::Status`] / a
    /// [`ClientError::Wire`] if the body is not exactly one byte of
    /// value `0` or `1`.
    pub async fn mode(&self) -> Result<bool, ClientError> {
        let m = self
            .send_measured(
                NetCall::Mode,
                self.http.get(format!("{}/mode", self.base)),
                0,
                MAX_TINY_BODY_BYTES,
                false,
            )
            .await?;
        match m.body.unwrap_or_default().as_slice() {
            [0] => Ok(false),
            [1] => Ok(true),
            other => Err(ClientError::Wire(format!(
                "/mode returned {} bytes (expected exactly one byte of 0 or 1)",
                other.len()
            ))),
        }
    }

    /// `GET /recent`: up to `node::RECENT_CAPACITY` recently-touched
    /// **addresses** (not key hashes), newest first, decoded from the
    /// server's `count:u32-le ‖ count × 20 bytes` framing.
    ///
    /// These are addresses from recent public blocks — the same public
    /// chain data anyone can read from any node — and are served
    /// identically to every caller, entirely separately from any query.
    /// Asking for this list tells the server nothing about which account
    /// a subsequent PIR query is for. See [`crate::node`]'s `recent`
    /// field docs.
    ///
    /// # Errors
    ///
    /// [`ClientError::Network`] / [`ClientError::Status`] / a
    /// [`ClientError::Wire`] if the body is shorter than its own
    /// declared length or is not `4 + 20·count` bytes exactly.
    pub async fn recent(&self) -> Result<Vec<[u8; 20]>, ClientError> {
        let m = self
            .send_measured(
                NetCall::Recent,
                self.http.get(format!("{}/recent", self.base)),
                0,
                MAX_RECENT_BODY_BYTES,
                false,
            )
            .await?;
        let bytes = m.body.unwrap_or_default();
        self.timed_decode(NetCall::Recent, || decode_recent(&bytes))
    }

    /// `GET /sync?from=<from>&to=<to>&epoch=<epoch>`: the coalesced delta
    /// for `(from, to]`, decoded via [`codec::decode_block_delta`].
    ///
    /// `plaintext_bits` / `arity` are this deployment's geometry, and
    /// `epoch` its lineage token ([`wire::lineage_epoch`] over the same
    /// bundle, ADR-0033) — all needed per call but not carried by
    /// `PirHttpClient` itself (see the struct docs); a caller
    /// (`risepir-rpc`'s `PrivateEth`) already has every one of them from
    /// the `SetupBundle` it bootstrapped from.
    ///
    /// # Returns
    ///
    /// `Ok(Some(delta))` on `200 OK`. `Ok(None)` on `409 Conflict` — the
    /// server's documented "requested range is outside the retained
    /// window / not this lineage" responses ([`crate::node`]'s `sync`
    /// handler docs); the caller must fall back to a full resync via
    /// [`Self::setup`], never treat this as "no change".
    ///
    /// # Errors
    ///
    /// [`ClientError::Network`] / [`ClientError::Status`] (any status
    /// other than `200`/`409`) / [`ClientError::Wire`] (a malformed
    /// `200`-body).
    pub async fn sync(
        &self,
        from: u64,
        to: u64,
        epoch: &str,
        plaintext_bits: u32,
        arity: u32,
    ) -> Result<Option<BlockDelta>, ClientError> {
        let m = self
            .send_measured(
                NetCall::Sync,
                self.http.get(format!(
                    "{}/sync?from={from}&to={to}&epoch={epoch}",
                    self.base
                )),
                0,
                MAX_SYNC_BODY_BYTES,
                true,
            )
            .await?;
        let Some(bytes) = m.body else {
            return Ok(None); // 409: outside the retained window / wrong lineage
        };
        let delta = self.timed_decode(NetCall::Sync, || {
            codec::decode_block_delta(&bytes, plaintext_bits, arity)
        })?;
        Ok(Some(delta))
    }

    /// `POST /answer?epoch=<epoch>`: send a per-segment query bundle
    /// (encoded via [`wire::encode_query_bundle`]) and decode the response
    /// bundle (via [`wire::decode_response_bundle`]).
    ///
    /// `reshape_row_width_per_seg` / `arity` are this deployment's
    /// geometry, needed only to decode the response, and `epoch` its
    /// lineage token (ADR-0033) — see [`Self::sync`]'s docs for why
    /// `PirHttpClient` takes these as arguments rather than storing them.
    ///
    /// When a [`NetSink`] is attached, the response's optional
    /// `x-risepir-answer-compute-ns` / `x-risepir-answer-handler-ns`
    /// headers are recorded into it. Both are optional: a server that
    /// does not publish them leaves the sink's values untouched, never
    /// zeroed.
    ///
    /// # Errors
    ///
    /// [`ClientError::Network`] / [`ClientError::Status`] (any status
    /// other than `200` — in particular, the server's `400 Bad Request`
    /// for a malformed query, and its `409 Conflict` for a stale-lineage
    /// `epoch`, both surface here; a caller that can re-bootstrap treats
    /// the `409` like a stalled sync) / [`ClientError::Wire`].
    pub async fn answer(
        &self,
        queries: &[SimpleQuery],
        epoch: &str,
        reshape_row_width_per_seg: &[u32],
        arity: usize,
    ) -> Result<(Vec<SimpleResponse>, u64), ClientError> {
        let body = wire::encode_query_bundle(queries);
        let request_bytes = body.len() as u64;
        let m = self
            .send_measured(
                NetCall::Answer,
                self.http
                    .post(format!("{}/answer?epoch={epoch}", self.base))
                    .body(body),
                request_bytes,
                MAX_ANSWER_BODY_BYTES,
                false,
            )
            .await?;
        if let Some(sink) = self.sink.as_ref() {
            sink.record_server_timing(
                header_u64(&m.headers, HDR_ANSWER_COMPUTE_NS),
                header_u64(&m.headers, HDR_ANSWER_HANDLER_NS),
            );
        }
        let bytes = m.body.unwrap_or_default();
        Ok(self.timed_decode(NetCall::Answer, || {
            wire::decode_response_bundle(&bytes, reshape_row_width_per_seg, arity)
        })?)
    }
}

/// Decode `GET /recent`'s `count:u32-le ‖ count × 20 bytes` framing.
///
/// The declared count is validated against the bytes actually present
/// *before* anything is allocated for it — this crate's standing rule,
/// and this body comes from a server the threat model does not ask the
/// client to trust for resource-safety.
fn decode_recent(bytes: &[u8]) -> Result<Vec<[u8; 20]>, ClientError> {
    let Some((len_bytes, rest)) = bytes.split_at_checked(4) else {
        return Err(ClientError::Wire(format!(
            "/recent returned {} bytes, too short for its 4-byte count prefix",
            bytes.len()
        )));
    };
    let count = u32::from_le_bytes(
        len_bytes
            .try_into()
            .expect("split_at_checked(4) yields exactly 4 bytes"),
    ) as usize;
    if rest.len() != count.saturating_mul(20) {
        return Err(ClientError::Wire(format!(
            "/recent declared {count} addresses but carries {} payload bytes (expected {})",
            rest.len(),
            count.saturating_mul(20)
        )));
    }
    Ok(rest
        .chunks_exact(20)
        .map(|c| {
            let mut a = [0u8; 20];
            a.copy_from_slice(c);
            a
        })
        .collect())
}

/// Nanoseconds since `t0`, saturated into a `u64` (~584 years of range).
fn elapsed_ns(t0: Instant) -> u64 {
    u64::try_from(t0.elapsed().as_nanos()).unwrap_or(u64::MAX)
}

/// One optional `u64`-valued response header. A header that is absent,
/// non-ASCII, or unparseable is `None` — never a defaulted zero, which
/// would be indistinguishable from a server that really reported zero.
fn header_u64(headers: &reqwest::header::HeaderMap, name: &str) -> Option<u64> {
    headers.get(name)?.to_str().ok()?.trim().parse().ok()
}

/// Shared `200`-or-`Err` handling: every method above except [`PirHttpClient::sync`]
/// (which has its own `409` case) treats any non-`200` status as
/// [`ClientError::Status`]. Reads the body to bytes only after the status
/// check passes, or best-effort as UTF-8 text for the error message
/// otherwise — never assumes an error body is even valid UTF-8.
///
/// `max_len` caps how much of the body is ever buffered (see the
/// transport-guardrails section above): the declared `Content-Length` is
/// checked before a single byte, and the streamed accumulation re-checks
/// as chunks arrive so a lying or absent header cannot bypass the cap.
async fn ok_body(resp: reqwest::Response, max_len: usize) -> Result<Vec<u8>, ClientError> {
    let status = resp.status();
    if status != reqwest::StatusCode::OK {
        let status_code = status.as_u16();
        let body = String::from_utf8_lossy(
            &read_capped(resp, MAX_ERROR_BODY_BYTES)
                .await
                .unwrap_or_default(),
        )
        .into_owned();
        return Err(ClientError::Status {
            status: status_code,
            body,
        });
    }
    if let Some(declared) = resp.content_length() {
        if declared > max_len as u64 {
            return Err(ClientError::Wire(format!(
                "declared response body of {declared} bytes exceeds this call's {max_len}-byte bound"
            )));
        }
    }
    let body = read_capped(resp, max_len).await?;
    Ok(body)
}

/// Streamed body accumulation with a hard byte cap — the transport-layer
/// half of "validate every length before allocating".
async fn read_capped(mut resp: reqwest::Response, max_len: usize) -> Result<Vec<u8>, ClientError> {
    let mut out = Vec::new();
    while let Some(chunk) = resp.chunk().await? {
        if out.len().saturating_add(chunk.len()) > max_len {
            return Err(ClientError::Wire(format!(
                "response body exceeded this call's {max_len}-byte bound mid-stream"
            )));
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_strips_trailing_slash() {
        let c = PirHttpClient::new("http://127.0.0.1:8645/");
        assert_eq!(c.base, "http://127.0.0.1:8645");
        let c2 = PirHttpClient::new("http://127.0.0.1:8645");
        assert_eq!(c2.base, "http://127.0.0.1:8645");
    }

    #[test]
    fn recent_decodes_newest_first_framing() {
        let mut body = 2u32.to_le_bytes().to_vec();
        body.extend_from_slice(&[0xaa; 20]);
        body.extend_from_slice(&[0xbb; 20]);
        assert_eq!(decode_recent(&body).unwrap(), vec![[0xaa; 20], [0xbb; 20]]);
        assert!(decode_recent(&0u32.to_le_bytes()).unwrap().is_empty());
    }

    #[test]
    fn recent_refuses_a_lying_or_truncated_count() {
        // Truncated prefix.
        assert!(matches!(
            decode_recent(&[0, 0, 0]),
            Err(ClientError::Wire(_))
        ));
        // A count far larger than the payload: must be refused before any
        // allocation is sized from it.
        let mut body = u32::MAX.to_le_bytes().to_vec();
        body.extend_from_slice(&[0x11; 20]);
        assert!(matches!(decode_recent(&body), Err(ClientError::Wire(_))));
        // A count one short of the payload.
        let mut body = 1u32.to_le_bytes().to_vec();
        body.extend_from_slice(&[0x11; 40]);
        assert!(matches!(decode_recent(&body), Err(ClientError::Wire(_))));
    }

    #[test]
    fn header_u64_is_none_unless_it_really_parses() {
        let mut h = reqwest::header::HeaderMap::new();
        assert_eq!(header_u64(&h, HDR_ANSWER_COMPUTE_NS), None);
        h.insert(
            HDR_ANSWER_COMPUTE_NS,
            reqwest::header::HeaderValue::from_static("  4321 "),
        );
        assert_eq!(header_u64(&h, HDR_ANSWER_COMPUTE_NS), Some(4321));
        h.insert(
            HDR_ANSWER_HANDLER_NS,
            reqwest::header::HeaderValue::from_static("not-a-number"),
        );
        assert_eq!(header_u64(&h, HDR_ANSWER_HANDLER_NS), None);
    }

    #[test]
    fn client_error_display_is_human_readable() {
        let e = ClientError::Status {
            status: 404,
            body: "nope".to_string(),
        };
        assert_eq!(e.to_string(), "unexpected HTTP status 404: nope");
        let e2 = ClientError::Wire("bad magic".to_string());
        assert_eq!(e2.to_string(), "wire decode error: bad magic");
    }
}
