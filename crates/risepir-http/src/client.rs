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
//! end (`risepir-rpc`) needs exactly that: a [`risepir_client::RisePirClient`]
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
//! [`risepir_client::RisePirClient`] or interprets a response itself. That
//! keeps the PIR *protocol* logic in exactly one place
//! (`risepir-client`) regardless of whether it is driven in-process or
//! over a real socket.

use ikpir_common::backend::simple::{SimpleQuery, SimpleResponse};
use ikpir_common::SimplePirBackend;
use risepir_proto::{codec, BlockDelta};
use risepir_server::SetupBundle;

use crate::wire::{self, WireError};

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
}

impl PirHttpClient {
    /// Build a client against `base` (e.g. `"http://127.0.0.1:8645"`,
    /// no trailing slash required — one is stripped if present).
    pub fn new(base: impl Into<String>) -> Self {
        let base = base.into();
        let base = base.strip_suffix('/').map(str::to_string).unwrap_or(base);
        Self {
            base,
            http: reqwest::Client::new(),
        }
    }

    /// `GET /setup`: the full [`SetupBundle`] a fresh
    /// [`risepir_client::RisePirClient`] bootstraps from.
    ///
    /// # Errors
    ///
    /// [`ClientError::Network`] / [`ClientError::Status`] (any status
    /// other than `200`) / [`ClientError::Wire`] (a malformed body — see
    /// [`wire::decode_setup`]).
    pub async fn setup(&self) -> Result<SetupBundle<SimplePirBackend>, ClientError> {
        let resp = self.http.get(format!("{}/setup", self.base)).send().await?;
        let bytes = ok_body(resp).await?;
        Ok(wire::decode_setup(&bytes)?)
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
        let resp = self.http.get(format!("{}/head", self.base)).send().await?;
        let bytes = ok_body(resp).await?;
        let arr: [u8; 8] = bytes
            .as_slice()
            .try_into()
            .map_err(|_| ClientError::Wire(format!("/head returned {} bytes, expected exactly 8", bytes.len())))?;
        Ok(u64::from_le_bytes(arr))
    }

    /// `GET /sync?from=<from>&to=<to>`: the coalesced delta for
    /// `(from, to]`, decoded via [`codec::decode_block_delta`].
    ///
    /// `plaintext_bits` / `arity` are this deployment's geometry — needed
    /// to decode the delta but not carried by `PirHttpClient` itself (see
    /// the struct docs); a caller (`risepir-rpc`'s `PrivateEth`) already
    /// has both from the `SetupBundle` it bootstrapped from.
    ///
    /// # Returns
    ///
    /// `Ok(Some(delta))` on `200 OK`. `Ok(None)` on `409 Conflict` — the
    /// server's documented "requested range is outside the retained
    /// window" response ([`crate::node`]'s `sync` handler docs); the
    /// caller must fall back to a full resync via [`Self::setup`], never
    /// treat this as "no change".
    ///
    /// # Errors
    ///
    /// [`ClientError::Network`] / [`ClientError::Status`] (any status
    /// other than `200`/`409`) / [`ClientError::Wire`] (a malformed
    /// `200`-body).
    pub async fn sync(&self, from: u64, to: u64, plaintext_bits: u32, arity: u32) -> Result<Option<BlockDelta>, ClientError> {
        let resp = self.http.get(format!("{}/sync?from={from}&to={to}", self.base)).send().await?;
        if resp.status() == reqwest::StatusCode::CONFLICT {
            return Ok(None);
        }
        let bytes = ok_body(resp).await?;
        let delta = codec::decode_block_delta(&bytes, plaintext_bits, arity)?;
        Ok(Some(delta))
    }

    /// `POST /answer`: send a per-segment query bundle (encoded via
    /// [`wire::encode_query_bundle`]) and decode the response bundle
    /// (via [`wire::decode_response_bundle`]).
    ///
    /// `reshape_row_width_per_seg` / `arity` are this deployment's
    /// geometry, needed only to decode the response — see [`Self::sync`]'s
    /// docs for why `PirHttpClient` takes these as arguments rather than
    /// storing them.
    ///
    /// # Errors
    ///
    /// [`ClientError::Network`] / [`ClientError::Status`] (any status
    /// other than `200` — in particular, the server's `400 Bad Request`
    /// for a malformed query surfaces here) / [`ClientError::Wire`].
    pub async fn answer(
        &self,
        queries: &[SimpleQuery],
        reshape_row_width_per_seg: &[u32],
        arity: usize,
    ) -> Result<(Vec<SimpleResponse>, u64), ClientError> {
        let body = wire::encode_query_bundle(queries);
        let resp = self.http.post(format!("{}/answer", self.base)).body(body).send().await?;
        let bytes = ok_body(resp).await?;
        Ok(wire::decode_response_bundle(&bytes, reshape_row_width_per_seg, arity)?)
    }
}

/// Shared `200`-or-`Err` handling: every method above except [`PirHttpClient::sync`]
/// (which has its own `409` case) treats any non-`200` status as
/// [`ClientError::Status`]. Reads the body to bytes only after the status
/// check passes, or best-effort as UTF-8 text for the error message
/// otherwise — never assumes an error body is even valid UTF-8.
async fn ok_body(resp: reqwest::Response) -> Result<Vec<u8>, ClientError> {
    let status = resp.status();
    if status != reqwest::StatusCode::OK {
        let status_code = status.as_u16();
        let body = resp.text().await.unwrap_or_default();
        return Err(ClientError::Status { status: status_code, body });
    }
    Ok(resp.bytes().await?.to_vec())
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
    fn client_error_display_is_human_readable() {
        let e = ClientError::Status { status: 404, body: "nope".to_string() };
        assert_eq!(e.to_string(), "unexpected HTTP status 404: nope");
        let e2 = ClientError::Wire("bad magic".to_string());
        assert_eq!(e2.to_string(), "wire decode error: bad magic");
    }
}
