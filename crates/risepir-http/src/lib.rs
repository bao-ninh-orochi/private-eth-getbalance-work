#![forbid(unsafe_code)]
#![warn(missing_docs)]
//! HTTP transport for the RisePIR private `eth_getBalance` service
//! (`docs/plan.md` §3.4, Stage 0.3): a SimplePIR-concrete binary wire codec
//! ([`wire`]) plus an axum server exposing it ([`node`]).
//!
//! # Two calls; the response names the epoch (ADR-0006)
//!
//! ```text
//! POST /answer            → (responses, block E')   # answered at the server's head
//! GET  /sync?from=&to=    → coalesced delta, or 409  # client resync convenience
//! GET  /delta/{block}     → one immutable per-block delta, cacheable forever
//! GET  /setup             → the full SetupBundle (params + backend_params + hints)
//! GET  /head              → the server's current block, as an 8-byte LE u64
//! ```
//!
//! `docs/plan.md` ADR-0006: raw HTTP + a purpose-built binary codec (never
//! JSON — base64 would inflate the ~400 KB query/response payloads
//! ~1.37×), and deltas are immutable per-block objects, which is exactly
//! what makes `/delta/{block}` cacheable forever
//! (`Cache-Control: public, max-age=31536000, immutable`).
//!
//! # Concurrency (ADR-0010)
//!
//! [`node::NodeState`] wraps the PIR server in a `tokio::sync::RwLock`: the
//! concrete `RisePirServer<Segmented2aryScheme, SimplePirBackend>` is
//! auto-`Send + Sync` (verified in `risepir-server`'s own test suite), so
//! the lock gives concurrent readers on the hot `/answer` path; the writer
//! ([`node::NodeState::apply_block`], called by the block-following driver,
//! not exposed over HTTP) takes it only once per block.
//!
//! # Security model — this is a security surface
//!
//! `POST /answer`'s body is attacker-controlled, multi-hundred-KB-scale
//! input. Every length or count this crate reads off the wire is validated
//! against the bytes actually remaining in the input *before* it is used
//! to size an allocation or index a slice — see [`wire`]'s module docs,
//! which mirrors the discipline `risepir_proto::codec` already
//! established, plus (going one step further, since this crate is the one
//! that feeds decoded values straight into `RisePirServer::answer`'s
//! per-segment matvec) an *exact*-length check on every segment's `Vec<u32>`
//! payload, not merely an upper bound — see [`wire`]'s per-function docs
//! for the out-of-bounds panic this closes downstream. Malformed,
//! truncated, or adversarial bytes always produce a clean `Err` (surfaced
//! over HTTP as `400 Bad Request`), never a panic, an OOM, or a silently
//! wrong response.
//!
//! # Scope
//!
//! Stage 0.3: the transport and its codec, server side ([`node`],
//! [`wire`]). Stage 0.4 adds the client side ([`client`]): an async
//! [`reqwest`]-based [`client::PirHttpClient`] that drives this same
//! transport over real HTTP, for callers (the JSON-RPC front end,
//! `risepir-rpc`) that need to reach a `NodeState` — local or remote —
//! from outside this process rather than via `tower::oneshot`.
//! Deliberately SimplePIR-concrete rather than generic over
//! `IncrementalPirBackend`: `docs/plan.md` ADR-0002 already commits this
//! deployment to SimplePIR (RisePIR-S), and a backend-generic wire format
//! would have to either erase `SimpleServerParams`' reshape geometry
//! (which has no Frodo equivalent) or grow a discriminant byte nothing
//! here needs yet.

//! # Feature gating (ADR-0019)
//!
//! [`wire`] is unconditional; [`node`] (axum) and [`client`] (reqwest) sit
//! behind the default-on `server` / `client` features. The browser client
//! takes this crate with `--no-default-features` so it shares the *exact*
//! codec the server encodes with — a second, hand-written JavaScript
//! decoder is precisely the kind of skew that produces a wrong answer
//! rather than a build error.

#[cfg(feature = "client")]
pub mod client;
// Optional wire-level instrumentation for `client` (per-endpoint call
// counts, wall time, byte counts) — a measurement concern, opt-in, and
// meaningless without the client that writes into it.
#[cfg(feature = "client")]
pub mod measure;
#[cfg(feature = "server")]
pub mod node;
// `/metrics` rendering (ADR-0039) — pure data + string building, used only
// by `node`'s handler; `pub(crate)` because nothing outside this crate
// needs it (the integration test in `tests/metrics.rs` drives the real
// `GET /metrics` HTTP route, not this module directly).
#[cfg(feature = "server")]
pub(crate) mod metrics;
#[cfg(feature = "server")]
pub mod web;
pub mod wire;

#[cfg(feature = "client")]
pub use client::{ClientError, PirHttpClient};
#[cfg(feature = "client")]
pub use measure::{CallStats, NetCall, NetSink, NetStats};
#[cfg(feature = "server")]
pub use node::{BlockApplyOutcome, NodeState, ReconcileHealth, MAX_ANSWER_BODY_BYTES};
#[cfg(feature = "server")]
pub use web::WebAssets;
pub use wire::WireError;
