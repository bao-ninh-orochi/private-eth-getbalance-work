//! The JSON-RPC 2.0 front end: `POST /`, deny-by-default for everything
//! except `eth_getBalance` / `eth_chainId` / `net_version` /
//! `eth_blockNumber` (`docs/plan.md` §3/§6, ADR-0007, ADR-0012).
//!
//! # Why hand-rolled JSON-RPC rather than a crate
//!
//! Every existing wire layer in this workspace is a purpose-built binary
//! codec (`risepir_proto::codec`, `risepir_http::wire`) specifically to
//! keep the deny/allow decision in this crate's own hands rather than a
//! generic RPC framework's. `serde_json` directly over an axum handler
//! gives the same full control at the HTTP boundary: this module decides
//! exactly which methods exist, exactly what each error looks like, and
//! exactly when to fall through to the opt-in proxy — nothing is
//! auto-dispatched by a macro that might one day auto-register a method
//! this deployment does not intend to expose.
//!
//! # Framing
//!
//! Every response is HTTP `200` with a JSON-RPC 2.0 envelope
//! (`{"jsonrpc":"2.0","id":..,"result":..}` xor `{..,"error":{"code":..,
//! "message":..}}`) — matching how `geth` and every common Ethereum
//! client library actually behaves (the *transport* status is `200`
//! whenever the JSON-RPC round trip itself succeeded; JSON-RPC-level
//! failures live inside the body). The one exception is the opt-in proxy
//! path, which returns the upstream's raw response — status, body, and
//! (best-effort) content type — completely unwrapped, since forwarding
//! promises the caller an unmodified upstream reply.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::error::RpcError;
use crate::private_eth::PrivateEth;

/// The only methods this deployment ever answers itself. Anything else
/// either 501s (no proxy configured) or is forwarded verbatim (proxy
/// configured) — see [`handle`].
const KNOWN_METHODS: [&str; 4] = ["eth_getBalance", "eth_chainId", "net_version", "eth_blockNumber"];

/// One JSON-RPC 2.0 error object (`{"code":.., "message":..}`).
struct JsonRpcError {
    code: i64,
    message: String,
}

impl JsonRpcError {
    fn new(code: i64, message: impl Into<String>) -> Self {
        Self { code, message: message.into() }
    }

    fn parse_error(detail: impl std::fmt::Display) -> Self {
        Self::new(-32700, format!("parse error: {detail}"))
    }

    fn invalid_request(detail: impl Into<String>) -> Self {
        Self::new(-32600, format!("invalid request: {}", detail.into()))
    }

    fn invalid_params(detail: impl Into<String>) -> Self {
        Self::new(-32602, format!("invalid params: {}", detail.into()))
    }

    /// Exact wording is load-bearing (documents the deny-by-default
    /// posture at the point a caller actually hits it, ADR-0012).
    fn method_not_found() -> Self {
        Self::new(
            -32601,
            "method not found (this is a private eth_getBalance endpoint; other methods are \
             denied by default — see --proxy-upstream)",
        )
    }

    /// Exact wording is load-bearing (`docs/plan.md` ADR-0007: our
    /// `"latest"` is our own head, not the public chain's — this is the
    /// caller-facing explanation of that gap for a block strictly older
    /// than our head).
    fn historical() -> Self {
        Self::new(-32000, "historical state not available (this endpoint serves head state only)")
    }

    /// The symmetric, unspecified-by-name-in-the-brief-but-obviously-needed
    /// case: a block *ahead* of our head. Same code as [`Self::historical`]
    /// (both are "not the one state we serve"), distinct message so an
    /// operator can tell the two apart from the error text alone.
    fn future_block(requested: u64, head: u64) -> Self {
        Self::new(
            -32000,
            format!("requested block {requested} is ahead of our head {head} (this endpoint serves head state only)"),
        )
    }

    fn internal(detail: impl std::fmt::Display) -> Self {
        Self::new(-32603, format!("internal error: {detail}"))
    }
}

impl From<RpcError> for JsonRpcError {
    fn from(e: RpcError) -> Self {
        match e {
            RpcError::DecodeFailed => Self::new(
                -32000,
                "value decode failed (checksum mismatch); refusing to return a possibly-corrupted balance",
            ),
            RpcError::Stalled => Self::new(
                -32000,
                "server is resyncing (the client fell behind the server's retained delta window); try again",
            ),
            RpcError::NotInTrackedSet => Self::new(
                -32000,
                "account not in tracked set (partial deployment: no complete snapshot loaded; only \
                 accounts whose balance changed since bootstrap are served — 0x0 would be a guess, \
                 and this endpoint never guesses)",
            ),
            other => Self::internal(other),
        }
    }
}

/// The two-element `[address, block]` params `eth_getBalance` expects,
/// both as JSON strings (per the JSON-RPC spec — a raw JSON number is
/// never valid here, so this type does not accept one either).
#[derive(Deserialize)]
struct GetBalanceParams(String, String);

/// Build the JSON-RPC axum router: a single `POST /`.
pub fn router(state: Arc<PrivateEth>) -> Router {
    Router::new().route("/", post(handle)).with_state(state)
}

/// `POST /`: the one JSON-RPC entry point. Never panics on malformed
/// input — every failure mode (invalid JSON, wrong shape, bad params, an
/// unknown method) maps to a clean JSON-RPC error object, per this
/// module's docs.
async fn handle(State(state): State<Arc<PrivateEth>>, body: Bytes) -> Response {
    let value: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => return Json(envelope(Value::Null, Err(JsonRpcError::parse_error(e)))).into_response(),
    };
    // `Value::get` returns `None` for any shape (not just a non-matching
    // key on an object) rather than panicking, so this is safe even if
    // `value` turns out not to be a JSON object at all.
    let id = value.get("id").cloned().unwrap_or(Value::Null);

    let (method, params) = match parse_method_and_params(&value) {
        Ok(t) => t,
        Err(e) => return Json(envelope(id, Err(e))).into_response(),
    };

    if !KNOWN_METHODS.contains(&method.as_str()) {
        return match state.proxy_upstream() {
            Some(upstream) => proxy_forward(state.proxy_http(), upstream, body).await,
            None => Json(envelope(id, Err(JsonRpcError::method_not_found()))).into_response(),
        };
    }

    let result = dispatch(&state, &method, &params).await;
    Json(envelope(id, result)).into_response()
}

/// Extracts `method` (required, must be a string) and `params` (optional,
/// defaults to an empty array) from a parsed JSON-RPC request body.
fn parse_method_and_params(value: &Value) -> Result<(String, Value), JsonRpcError> {
    if !value.is_object() {
        return Err(JsonRpcError::invalid_request("expected a JSON object"));
    }
    let method = value
        .get("method")
        .and_then(Value::as_str)
        .ok_or_else(|| JsonRpcError::invalid_request("missing or non-string \"method\""))?
        .to_string();
    let params = value.get("params").cloned().unwrap_or_else(|| Value::Array(Vec::new()));
    Ok((method, params))
}

/// Dispatch one of [`KNOWN_METHODS`] to its handler.
async fn dispatch(state: &PrivateEth, method: &str, params: &Value) -> Result<Value, JsonRpcError> {
    match method {
        "eth_getBalance" => eth_get_balance(state, params).await,
        "eth_chainId" => Ok(Value::String(format!("0x{:x}", state.chain_id()))),
        "net_version" => Ok(Value::String(state.chain_id().to_string())),
        "eth_blockNumber" => {
            let head = state.head().await?;
            Ok(Value::String(format!("0x{head:x}")))
        }
        // Unreachable given `handle`'s `KNOWN_METHODS` guard, but kept as
        // a proper `Err` rather than `unreachable!()` — this module's
        // whole point is that adversarial/unexpected input never panics,
        // and a defensive branch is cheaper than trusting a future
        // refactor to preserve that guard.
        _ => Err(JsonRpcError::method_not_found()),
    }
}

/// `eth_getBalance(address, block)`: parse both params, validate the
/// block selector against our head (`docs/plan.md` ADR-0007), then the
/// private lookup itself.
async fn eth_get_balance(state: &PrivateEth, params: &Value) -> Result<Value, JsonRpcError> {
    let GetBalanceParams(addr_str, block_str) = serde_json::from_value(params.clone())
        .map_err(|e| JsonRpcError::invalid_params(format!("eth_getBalance expects params [address, block]: {e}")))?;

    let addr20 =
        parse_address(&addr_str).ok_or_else(|| JsonRpcError::invalid_params("address must be a 0x-prefixed 20-byte hex string"))?;

    let head = state.head().await?;
    classify_block_param(&block_str, head)?;

    let balance = state.get_balance(addr20).await?;
    // Ethereum's `eth_getBalance` returns a hex-encoded wei quantity with
    // minimal digits and no leading zeros; `{:x}` on a `u128` already
    // produces exactly that (including "0" for zero, never "00").
    Ok(Value::String(format!("0x{balance:x}")))
}

/// Parses a `0x`-prefixed, exactly-40-hex-digit Ethereum address.
fn parse_address(s: &str) -> Option<[u8; 20]> {
    let hex = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X"))?;
    if hex.len() != 40 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut out = [0u8; 20];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

/// Validates `eth_getBalance`'s block selector against `head` (this
/// deployment's own head — ADR-0007's `"latest"`, not any public chain's).
/// `Ok(())` for a tag or a hex number equal to `head`; an error for
/// anything else (historical, future, or unparseable) — never silently
/// treated as "close enough".
fn classify_block_param(s: &str, head: u64) -> Result<(), JsonRpcError> {
    match s {
        "latest" | "pending" | "safe" | "finalized" => Ok(()),
        _ => {
            let hex = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")).ok_or_else(|| {
                JsonRpcError::invalid_params(format!(
                    "block must be one of latest|pending|safe|finalized or a 0x-hex quantity, got {s:?}"
                ))
            })?;
            let n = u64::from_str_radix(hex, 16).map_err(|_| {
                JsonRpcError::invalid_params(format!(
                    "block must be one of latest|pending|safe|finalized or a 0x-hex quantity, got {s:?}"
                ))
            })?;
            match n.cmp(&head) {
                std::cmp::Ordering::Equal => Ok(()),
                std::cmp::Ordering::Less => Err(JsonRpcError::historical()),
                std::cmp::Ordering::Greater => Err(JsonRpcError::future_block(n, head)),
            }
        }
    }
}

/// Wraps a dispatch result in the JSON-RPC 2.0 envelope: `result` XOR
/// `error`, `id` always echoed back verbatim.
fn envelope(id: Value, result: Result<Value, JsonRpcError>) -> Value {
    match result {
        Ok(v) => json!({"jsonrpc": "2.0", "id": id, "result": v}),
        Err(e) => json!({"jsonrpc": "2.0", "id": id, "error": {"code": e.code, "message": e.message}}),
    }
}

/// Forwards `body` (the original, untouched request bytes) to
/// `upstream` and returns its response verbatim — status, body, and
/// (best-effort) `Content-Type` — never re-wrapped in this crate's own
/// envelope (`docs/plan.md` ADR-0012's opt-in leak path: the caller was
/// promised the *upstream's* answer, unmodified).
async fn proxy_forward(http: &reqwest::Client, upstream: &str, body: Bytes) -> Response {
    let sent = http
        .post(upstream)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(body.to_vec())
        .send()
        .await;

    let resp = match sent {
        Ok(r) => r,
        Err(e) => return (StatusCode::BAD_GATEWAY, format!("proxy_upstream request failed: {e}")).into_response(),
    };

    let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);

    match resp.bytes().await {
        Ok(bytes) => {
            let mut out = (status, bytes.to_vec()).into_response();
            if let Some(ct) = content_type {
                if let Ok(header_value) = axum::http::HeaderValue::from_str(&ct) {
                    out.headers_mut().insert(axum::http::header::CONTENT_TYPE, header_value);
                }
            }
            out
        }
        Err(e) => (StatusCode::BAD_GATEWAY, format!("proxy_upstream response body read failed: {e}")).into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_address_accepts_valid_and_rejects_malformed() {
        assert_eq!(parse_address("0x1111111111111111111111111111111111111111"), Some([0x11u8; 20]));
        assert_eq!(parse_address("0X2222222222222222222222222222222222222222"), Some([0x22u8; 20]));
        assert_eq!(parse_address(""), None);
        assert_eq!(parse_address("0x1234"), None, "too short");
        assert_eq!(parse_address("111111111111111111111111111111111111111a"), None, "missing 0x");
        assert_eq!(
            parse_address("0xzzzz111111111111111111111111111111111111"),
            None,
            "non-hex digits"
        );
    }

    #[test]
    fn classify_block_param_accepts_known_tags() {
        for tag in ["latest", "pending", "safe", "finalized"] {
            assert!(classify_block_param(tag, 42).is_ok(), "{tag} must be accepted at any head");
        }
    }

    #[test]
    fn classify_block_param_accepts_exact_head_only() {
        assert!(classify_block_param("0x2a", 42).is_ok(), "0x2a == 42 must be accepted");
        assert!(classify_block_param("0x29", 42).is_err(), "41 < 42 must be rejected (historical)");
        assert!(classify_block_param("0x2b", 42).is_err(), "43 > 42 must be rejected (future)");
    }

    #[test]
    fn classify_block_param_rejects_garbage() {
        assert!(classify_block_param("earliest", 42).is_err(), "not one of the four accepted tags");
        assert!(classify_block_param("banana", 42).is_err());
        assert!(classify_block_param("0xzz", 42).is_err());
    }

    #[test]
    fn envelope_is_result_xor_error() {
        let ok = envelope(json!(1), Ok(json!("0x1")));
        assert_eq!(ok["result"], json!("0x1"));
        assert!(ok.get("error").is_none());
        assert_eq!(ok["id"], json!(1));

        let err = envelope(Value::Null, Err(JsonRpcError::method_not_found()));
        assert!(err.get("result").is_none());
        assert_eq!(err["error"]["code"], json!(-32601));
        assert_eq!(err["id"], Value::Null);
    }
}
