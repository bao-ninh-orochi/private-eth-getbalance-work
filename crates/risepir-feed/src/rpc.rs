//! [`RpcFeed`] — the real-mainnet [`crate::Feed`] counterpart: one
//! [`BlockUpdate`] per finalized block from a public JSON-RPC endpoint
//! (`docs/sync.md`; ADR-0007: follow `finalized`, ADR-0018: withdrawals
//! ride `BlockUpdate::credits`).
//!
//! # Where each piece of a block's balance changes comes from
//!
//! - **Transaction effects** — `debug_traceBlockByNumber(N,
//!   prestateTracer, diffMode)`: per transaction, `post` holds the *new*
//!   values of every field that changed, `pre` the old. The rules this
//!   module applies (unit-tested on canned fixtures below, and checked
//!   live against a second provider's `eth_getBalance` by the `--ignored`
//!   tests):
//!   - account in `post` **with** a `balance` field → that hex quantity is
//!     the account's post-transaction balance;
//!   - account in `post` **without** a `balance` field (e.g. only nonce or
//!     storage changed) → balance untouched by this tx — leave whatever an
//!     earlier tx (or nothing) established;
//!   - account in `pre` but **absent from `post` entirely** → destroyed
//!     (self-destruct) → balance `0`;
//!   - later transactions override earlier ones (last-write-wins per
//!     address across the block).
//! - **Beacon withdrawals (EIP-4895)** — *not* visible to any tracer;
//!   merged from the block body's `withdrawals[]`, each `amount` a hex
//!   **gwei** quantity → `× 10⁹` wei, emitted as relative
//!   [`BlockUpdate::credits`] that `apply_block` resolves against the
//!   store's own verified prior (ADR-0018).
//!
//! # Trust posture
//!
//! The feed endpoint is trusted for *data* (it is the balance oracle by
//! construction) but never for *shape*: every hex quantity, every field
//! access is checked, and anything malformed is a loud [`FeedError`],
//! never a guessed value. Cross-provider reconciliation (`docs/sync.md`;
//! the binary's follow loop) is what bounds a lying/buggy endpoint: store
//! values are periodically diffed against an independent provider's
//! `eth_getBalance` at the same height, and any mismatch halts serving.

use std::collections::BTreeMap;

use risepir_proto::{keccak256, Balance, BlockUpdate};
use serde_json::{json, Value};

use crate::FeedError;

/// Wei per gwei — withdrawal `amount`s are gwei on the wire (EIP-4895).
const WEI_PER_GWEI: u128 = 1_000_000_000;

/// A 20-byte Ethereum address (pre-keccak — [`BlockUpdate`] carries the
/// hashed form; reconciliation needs the raw address to ask a reference
/// RPC about it).
pub type Address = [u8; 20];

/// Minimal async JSON-RPC 2.0 client over `reqwest` — one POST per call,
/// no batching, no retries (the follow loop's own cadence is the retry:
/// re-asking for the same finalized block is idempotent).
pub struct RpcClient {
    url: String,
    http: reqwest::Client,
}

/// What this client identifies itself as. **Load-bearing, not cosmetic:**
/// `reqwest` sends no `User-Agent` at all by default, and Cloudflare-fronted
/// public RPC endpoints reject that outright — `eth.merkle.io` answers a
/// UA-less POST with a `403` HTML challenge page while answering the
/// identical request with any UA set. Diagnosing that costs an afternoon,
/// because every hand-check with `curl` (which always sends one) succeeds
/// and only the application sees the 403.
const USER_AGENT: &str = concat!("risepir-rpc/", env!("CARGO_PKG_VERSION"));

/// How long to wait for a TCP+TLS connection to the endpoint.
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// How long the endpoint may send **nothing at all** before the call is
/// abandoned (ADR-0035).
///
/// This is the server-side half of the same defect that wedged the browser
/// page: "no retries — the follow loop's own cadence is the retry" (above)
/// is only true for a call that *returns*. Without these two bounds a
/// half-open socket to the feed — the far side gone without a FIN, which
/// is ordinary behaviour for a public endpoint behind a load balancer —
/// left `finalized()` or `block_update()` awaiting forever, and the follow
/// loop with it: no error, so no retry, no `critical`, no log line. The
/// server would simply stop following the chain while continuing to answer
/// `/setup` and `/answer` from a frozen head, and the only outward sign
/// would be the front end's own "stalled at block N" (`web/app.js`) 15
/// minutes later.
///
/// It bounds *silence*, not total duration, so a legitimately slow
/// `trace_block` on an archive endpoint is never cut off mid-answer — only
/// one that has gone quiet. 60 s rather than the Rust PIR client's 30 s
/// (`READ_STALL_TIMEOUT`, crates/risepir-http/src/client.rs) because these
/// are heavy archive calls against a keyless public endpoint, and a false
/// timeout here is cheap anyway: the loop re-asks for the same finalized
/// block, which is idempotent by construction.
const READ_STALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

impl RpcClient {
    /// A client for `url`. No I/O yet.
    ///
    /// # Panics
    ///
    /// If the HTTP client cannot be built — only possible from a broken
    /// TLS backend, which is a deployment-environment failure rather than
    /// anything this crate can recover from.
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            http: reqwest::Client::builder()
                .user_agent(USER_AGENT)
                .connect_timeout(CONNECT_TIMEOUT)
                .read_timeout(READ_STALL_TIMEOUT)
                .build()
                .expect("reqwest client with a static user-agent"),
        }
    }

    /// The endpoint URL this client talks to.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// One JSON-RPC call. `Ok` is the `result` member; a transport failure,
    /// non-2xx status, JSON-RPC `error` member, or missing `result` is a
    /// [`FeedError::Rpc`] — except one that looks like the endpoint
    /// refusing to serve state at this depth/height, which is a
    /// [`FeedError::DepthRefused`] instead (ADR-0036; see that variant's
    /// docs for the classification heuristic and why misclassifying it is
    /// safe). A transport failure (no response received at all) is never
    /// classified either way — there is no status or body to classify.
    pub async fn call(&self, method: &str, params: Value) -> Result<Value, FeedError> {
        let body = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
        let rpc_err = |detail: String| FeedError::Rpc {
            method: method.to_string(),
            detail,
        };
        // Picks `DepthRefused` over the plain `Rpc` above whenever the
        // heuristic matches — same `method`, same `detail` text either way,
        // so this changes nothing a caller sees via `Display`, only which
        // variant `is_depth_refusal()` reports.
        let classified_err = |status: u16, code: Option<i64>, message: &str, detail: String| {
            if looks_like_depth_refusal(status, code, message) {
                FeedError::DepthRefused {
                    method: method.to_string(),
                    detail,
                }
            } else {
                FeedError::Rpc {
                    method: method.to_string(),
                    detail,
                }
            }
        };

        let resp = self
            .http
            .post(&self.url)
            .json(&body)
            .send()
            .await
            .map_err(|e| rpc_err(format!("transport: {e}")))?;
        let status = resp.status();
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| rpc_err(format!("body read: {e}")))?;
        if !status.is_success() {
            // Best-effort: publicnode's own archive-depth refusal is a
            // bare `{"code":...,"message":...}` body on a `403`, which
            // `extract_json_rpc_code_message` reads directly; the `403`
            // alone is already sufficient for `looks_like_depth_refusal`
            // even when a *different* endpoint's non-2xx body is not JSON
            // at all.
            let (code, message) = extract_json_rpc_code_message(&bytes);
            let detail = format!(
                "HTTP {status}: {}",
                String::from_utf8_lossy(&bytes[..bytes.len().min(200)])
            );
            return Err(classified_err(status.as_u16(), code, &message, detail));
        }
        let mut envelope: Value =
            serde_json::from_slice(&bytes).map_err(|e| rpc_err(format!("invalid JSON: {e}")))?;
        if let Some(err) = envelope.get("error") {
            // The shape a full (non-archive) node uses for its classic
            // "missing trie node" / "pruned" refusal: a `200` carrying a
            // JSON-RPC `error` member rather than a non-2xx status.
            let code = err.get("code").and_then(Value::as_i64);
            let message = err.get("message").and_then(Value::as_str).unwrap_or("");
            let detail = format!("JSON-RPC error: {err}");
            return Err(classified_err(status.as_u16(), code, message, detail));
        }
        match envelope.get_mut("result") {
            Some(r) => Ok(r.take()),
            None => Err(rpc_err("response has neither result nor error".to_string())),
        }
    }

    /// `eth_getBalance(addr, block)` — used by reconciliation (against a
    /// *different* provider than the feed's own) and by the bootstrap-seam
    /// check. Errors are loud; a reconciler must never treat "could not
    /// fetch" as "matched".
    pub async fn balance_at(&self, addr: &Address, block: u64) -> Result<Balance, FeedError> {
        let addr_hex = format!("0x{}", hex(addr));
        let result = self
            .call("eth_getBalance", json!([addr_hex, format!("0x{block:x}")]))
            .await?;
        let s = result
            .as_str()
            .ok_or_else(|| parse_err("eth_getBalance", "result is not a string"))?;
        parse_hex_u128(s).map_err(|e| parse_err("eth_getBalance", &e))
    }
}

/// One configured endpoint plus whether its `eth_chainId` has been
/// confirmed to match the deployment's chain.
///
/// A fallback that was unreachable at startup stays in the list
/// *unverified*; it is re-checked before its data is ever accepted (see
/// [`RpcFeed::ensure_chain_verified`]), so "temporarily down" never
/// becomes "silently trusted".
struct Endpoint {
    rpc: RpcClient,
    chain_verified: std::sync::atomic::AtomicBool,
}

/// The real-mainnet feed: follows `finalized` over an ordered list of
/// JSON-RPC endpoints that serve `debug_traceBlockByNumber` with the
/// prestate tracer (verified live: dRPC's keyless `https://eth.drpc.org`
/// does, with `https://eth.merkle.io` behind it for the blocks dRPC's
/// free plan refuses).
///
/// Not an implementation of the synchronous [`crate::Feed`] trait — this
/// type is async through and through; the binary's follow loop drives it
/// directly. The one seam shared with every other producer is the
/// [`BlockUpdate`] it emits.
pub struct RpcFeed {
    /// Ordered endpoints: `endpoints[0]` is the primary, the rest are
    /// fallbacks tried in order. Never empty.
    endpoints: Vec<Endpoint>,
    chain_id: u64,
}

impl RpcFeed {
    /// Connect-and-verify a single endpoint. Equivalent to
    /// [`Self::new_multi`] with a one-element list.
    pub async fn new(url: impl Into<String>, expected_chain_id: u64) -> Result<Self, FeedError> {
        Self::new_multi(vec![url.into()], expected_chain_id).await
    }

    /// Connect-and-verify an **ordered** endpoint list: `urls[0]` is the
    /// primary, the rest are fallbacks tried in order whenever a call on
    /// an earlier one fails.
    ///
    /// # Why fallbacks exist
    ///
    /// A single endpoint is a single point of *permanent* failure, not
    /// just a slow one. Keyless public providers refuse individual blocks
    /// on plan limits — dRPC answers `debug_traceBlockByNumber` for most
    /// blocks in ~1 s but returns `HTTP 408 "Request timeout on the free
    /// plan"` for occasional heavy ones, deterministically, however many
    /// times it is asked. The follow loop must never skip a block (a
    /// skipped block is a wrong balance), so it retries forever — and a
    /// deployment following mainnet wedges permanently on the first such
    /// block. Observed live on 2026-07-26: the complete-set deployment
    /// stopped at block 25,613,828 and stayed there through 55 identical
    /// retries. A second endpoint that *does* serve that block clears it
    /// in one call.
    ///
    /// Fallbacks are consulted per call, in order, and the primary is
    /// always tried first — so a rate-limited-but-permissive endpoint is
    /// a perfectly good fallback: it is asked only for the rare block the
    /// primary refuses.
    ///
    /// # Startup strictness, and why it differs by position
    ///
    /// A **chain-id mismatch is always fatal**, wherever it appears: an
    /// endpoint serving another chain's blocks would corrupt the database,
    /// and that is a misconfiguration no retry fixes.
    ///
    /// An endpoint being merely *unreachable* is treated by position:
    ///
    /// - **primary** — fatal, unchanged: a deployment that cannot reach
    ///   its feed is dead on arrival.
    /// - **fallback** — a warning. It stays in the list, unverified, and
    ///   its chain id is checked before its data is ever used. Killing a
    ///   deployment because a *backup* endpoint had a bad minute would
    ///   make the fallback mechanism worse than no fallback at all —
    ///   which is exactly what happened on 2026-07-26, when a transient
    ///   `403` from the fallback aborted startup on a server holding a
    ///   36 GB state file it had spent 33 minutes building.
    ///
    /// # Errors
    ///
    /// [`FeedError::Internal`] if `urls` is empty;
    /// [`FeedError::ChainIdMismatch`] if any endpoint reports the wrong
    /// chain; whatever the primary's `eth_chainId` failed with, if it did.
    pub async fn new_multi(urls: Vec<String>, expected_chain_id: u64) -> Result<Self, FeedError> {
        if urls.is_empty() {
            return Err(FeedError::Internal("feed needs at least one endpoint URL".to_string()));
        }
        let mut endpoints = Vec::with_capacity(urls.len());
        for (i, url) in urls.into_iter().enumerate() {
            let rpc = RpcClient::new(url);
            let verified = match verify_chain(&rpc, expected_chain_id).await {
                Ok(()) => true,
                // Wrong chain: fatal at any position.
                Err(e @ FeedError::ChainIdMismatch { .. }) => return Err(e),
                // Unreachable primary: fatal, as before.
                Err(e) if i == 0 => return Err(e),
                // Unreachable fallback: keep it, unverified.
                Err(e) => {
                    eprintln!(
                        "risepir-feed: WARNING: fallback feed endpoint {} did not verify at startup ({e}); \
                         keeping it in the chain — its chain id will be re-checked before any of its data is used",
                        rpc.url()
                    );
                    false
                }
            };
            endpoints.push(Endpoint {
                rpc,
                chain_verified: std::sync::atomic::AtomicBool::new(verified),
            });
        }
        Ok(Self {
            endpoints,
            chain_id: expected_chain_id,
        })
    }

    /// Confirms `ep`'s chain id if startup could not. Cheap after the
    /// first success (one relaxed atomic load).
    async fn ensure_chain_verified(&self, ep: &Endpoint) -> Result<(), FeedError> {
        use std::sync::atomic::Ordering;
        if ep.chain_verified.load(Ordering::Relaxed) {
            return Ok(());
        }
        verify_chain(&ep.rpc, self.chain_id).await?;
        ep.chain_verified.store(true, Ordering::Relaxed);
        Ok(())
    }

    /// The verified chain id this feed was constructed against.
    pub const fn chain_id(&self) -> u64 {
        self.chain_id
    }

    /// The **primary** JSON-RPC client (for ad-hoc calls like the
    /// bootstrap-seam `eth_getBalance`).
    pub fn rpc(&self) -> &RpcClient {
        &self.endpoints[0].rpc
    }

    /// Every configured endpoint URL, primary first — what the deployment
    /// logs at startup so the operator can see the actual fallback order.
    pub fn urls(&self) -> Vec<&str> {
        self.endpoints.iter().map(|e| e.rpc.url()).collect()
    }

    /// Current `finalized` block number (ADR-0007). Tries each endpoint
    /// in order; see [`Self::new_multi`].
    pub async fn finalized(&self) -> Result<u64, FeedError> {
        let mut failures = Vec::new();
        for ep in &self.endpoints {
            if let Err(e) = self.ensure_chain_verified(ep).await {
                failures.push(format!("{}: unverified ({e})", ep.rpc.url()));
                continue;
            }
            match Self::finalized_on(&ep.rpc).await {
                Ok(n) => return Ok(n),
                Err(e) => failures.push(format!("{}: {e}", ep.rpc.url())),
            }
        }
        Err(all_failed("eth_getBlockByNumber(finalized)", &failures))
    }

    async fn finalized_on(rpc: &RpcClient) -> Result<u64, FeedError> {
        let block = rpc
            .call("eth_getBlockByNumber", json!(["finalized", false]))
            .await?;
        let num = block
            .get("number")
            .and_then(Value::as_str)
            .ok_or_else(|| parse_err("eth_getBlockByNumber(finalized)", "missing number"))?;
        let num = parse_hex_u128(num).map_err(|e| parse_err("eth_getBlockByNumber(finalized)", &e))?;
        u64::try_from(num).map_err(|_| parse_err("eth_getBlockByNumber(finalized)", "block number does not fit u64"))
    }

    /// One finalized block's complete [`BlockUpdate`]: traced transaction
    /// effects as absolute `changes`, EIP-4895 withdrawals as relative
    /// `credits`, keys already `keccak256`-hashed (ADR-0008). The raw
    /// (pre-hash) addresses ride alongside in [`FetchedBlock`] — the
    /// reconciliation loop needs them to ask a reference RPC, and a
    /// partial-mode deployment needs the credit recipients to filter
    /// credits it cannot honestly resolve (no complete prior).
    ///
    /// Tries each configured endpoint in order (see [`Self::new_multi`]);
    /// the returned error names every endpoint and why each declined, so
    /// a genuinely unavailable block is distinguishable from one provider
    /// having a bad day.
    pub async fn block_update(&self, n: u64) -> Result<FetchedBlock, FeedError> {
        let mut failures = Vec::new();
        for ep in &self.endpoints {
            if let Err(e) = self.ensure_chain_verified(ep).await {
                failures.push(format!("{}: unverified ({e})", ep.rpc.url()));
                continue;
            }
            match Self::block_update_on(&ep.rpc, n).await {
                Ok(b) => return Ok(b),
                Err(e) => failures.push(format!("{}: {e}", ep.rpc.url())),
            }
        }
        Err(all_failed(&format!("block_update({n})"), &failures))
    }

    async fn block_update_on(rpc: &RpcClient, n: u64) -> Result<FetchedBlock, FeedError> {
        let hex_n = format!("0x{n:x}");

        let block = rpc
            .call("eth_getBlockByNumber", json!([hex_n.clone(), false]))
            .await?;
        if block.is_null() {
            return Err(FeedError::Rpc {
                method: "eth_getBlockByNumber".to_string(),
                detail: format!("block {n} not available on this endpoint"),
            });
        }
        let credited = credits_from_block(&block)?;

        let trace = rpc
            .call(
                "debug_traceBlockByNumber",
                json!([hex_n, {"tracer": "prestateTracer", "tracerConfig": {"diffMode": true}}]),
            )
            .await?;
        let changed = changes_from_trace(&trace)?;

        let changes = changed.iter().map(|(a, b)| (keccak256(a), *b)).collect();
        let credits = credited.iter().map(|(a, b)| (keccak256(a), *b)).collect();
        Ok(FetchedBlock {
            update: BlockUpdate {
                block: n,
                changes,
                credits,
            },
            changed,
            credited,
        })
    }
}

/// What [`RpcFeed::block_update`] hands back: the hashed-key
/// [`BlockUpdate`] the server consumes, plus the raw-address views the
/// deployment's own loops need (reconciliation sampling, partial-mode
/// credit filtering). `changed`/`credited` are exactly the pre-hash
/// counterparts of `update.changes`/`update.credits`, index-aligned.
pub struct FetchedBlock {
    /// The server-facing update (keys `keccak256`-hashed, ADR-0008).
    pub update: BlockUpdate,
    /// Raw `(address, post-block balance)` per tx-changed account.
    pub changed: Vec<(Address, Balance)>,
    /// Raw `(address, amount_wei)` per withdrawal, block order, duplicates
    /// kept.
    pub credited: Vec<(Address, Balance)>,
}

// ─── Pure parsers (unit-tested on canned fixtures; no I/O) ──────────────

/// Applies the diffMode rules (module docs) to a
/// `debug_traceBlockByNumber` result: `(address, post-block balance)`
/// per address whose balance any transaction changed, deterministic
/// order, one entry per address (last transaction wins).
pub fn changes_from_trace(trace: &Value) -> Result<Vec<(Address, Balance)>, FeedError> {
    let txs = trace
        .as_array()
        .ok_or_else(|| parse_err("debug_traceBlockByNumber", "result is not an array"))?;

    let mut map: BTreeMap<Address, Balance> = BTreeMap::new();
    for (i, tx) in txs.iter().enumerate() {
        let ctx = |what: &str| format!("tx[{i}]: {what}");
        let result = tx
            .get("result")
            .ok_or_else(|| parse_err("debug_traceBlockByNumber", &ctx("missing result")))?;
        if let Some(err) = result.get("error").and_then(Value::as_str) {
            // A tracer-level failure for one tx means the block's change
            // set is unknowable — never "skip and hope".
            return Err(parse_err("debug_traceBlockByNumber", &ctx(&format!("tracer error: {err}"))));
        }
        let pre = result
            .get("pre")
            .and_then(Value::as_object)
            .ok_or_else(|| parse_err("debug_traceBlockByNumber", &ctx("missing pre object")))?;
        let post = result
            .get("post")
            .and_then(Value::as_object)
            .ok_or_else(|| parse_err("debug_traceBlockByNumber", &ctx("missing post object")))?;

        for (addr_str, acct) in post {
            let addr = parse_address(addr_str)
                .ok_or_else(|| parse_err("debug_traceBlockByNumber", &ctx("bad post address")))?;
            match acct.get("balance") {
                Some(b) => {
                    let b = b
                        .as_str()
                        .ok_or_else(|| parse_err("debug_traceBlockByNumber", &ctx("post balance not a string")))?;
                    let wei = parse_hex_u128(b)
                        .map_err(|e| parse_err("debug_traceBlockByNumber", &ctx(&e)))?;
                    map.insert(addr, wei);
                }
                None => { /* balance untouched by this tx */ }
            }
        }
        for (addr_str, _) in pre {
            if !post.contains_key(addr_str) {
                // In pre, absent from post: destroyed this tx.
                let addr = parse_address(addr_str)
                    .ok_or_else(|| parse_err("debug_traceBlockByNumber", &ctx("bad pre address")))?;
                map.insert(addr, 0);
            }
        }
    }
    Ok(map.into_iter().collect())
}

/// Extracts EIP-4895 withdrawal credits from a block body:
/// `(recipient, amount_wei)` per entry, in block order, duplicates kept
/// (they accumulate in `apply_block`). Pre-Shanghai blocks (no
/// `withdrawals` field) yield an empty list.
pub fn credits_from_block(block: &Value) -> Result<Vec<(Address, Balance)>, FeedError> {
    let Some(withdrawals) = block.get("withdrawals") else {
        return Ok(Vec::new());
    };
    let withdrawals = withdrawals
        .as_array()
        .ok_or_else(|| parse_err("eth_getBlockByNumber", "withdrawals is not an array"))?;

    let mut out = Vec::with_capacity(withdrawals.len());
    for (i, w) in withdrawals.iter().enumerate() {
        let ctx = |what: &str| format!("withdrawals[{i}]: {what}");
        let addr = w
            .get("address")
            .and_then(Value::as_str)
            .and_then(parse_address_str)
            .ok_or_else(|| parse_err("eth_getBlockByNumber", &ctx("bad address")))?;
        let gwei = w
            .get("amount")
            .and_then(Value::as_str)
            .ok_or_else(|| parse_err("eth_getBlockByNumber", &ctx("missing amount")))?;
        let gwei = parse_hex_u128(gwei).map_err(|e| parse_err("eth_getBlockByNumber", &ctx(&e)))?;
        let wei = gwei
            .checked_mul(WEI_PER_GWEI)
            .ok_or_else(|| parse_err("eth_getBlockByNumber", &ctx("amount overflows wei")))?;
        // amount == 0 never occurs in real blocks but would be a harmless
        // no-op credit; kept rather than special-cased.
        out.push((addr, wei));
    }
    Ok(out)
}

// ─── small helpers ──────────────────────────────────────────────────────

/// Heuristic: does `(status, code, message)` look like the endpoint
/// refusing to serve state at the requested depth, rather than an ordinary
/// transport hiccup or a plain rate limit? See [`FeedError::DepthRefused`]'s
/// docs for why misclassifying here is safe — it only ever changes request
/// volume and log lines, never whether a mismatch is detected.
///
/// - `status == 403` alone is sufficient — publicnode's live archive-depth
///   refusal is exactly this (`HTTP 403 {"code":-32602,"message":"Archive
///   requests require a personal token..."}`), independent of whether the
///   body parsed at all.
/// - Otherwise, the JSON-RPC error `code` must be `-32602` or `-32000`
///   *and* `message` must contain (case-insensitively) one of a handful of
///   known archive/pruning phrases. A plain `-32000` with an unrelated
///   message (e.g. "execution reverted") does not qualify, and neither does
///   a rate limit under a different code (e.g. `429`/`-32005`).
fn looks_like_depth_refusal(status: u16, code: Option<i64>, message: &str) -> bool {
    if status == 403 {
        return true;
    }
    if code == Some(-32602) || code == Some(-32000) {
        const NEEDLES: [&str; 5] = [
            "archive",
            "missing trie node",
            "state is not available",
            "state unavailable",
            "pruned",
        ];
        let lower = message.to_ascii_lowercase();
        return NEEDLES.iter().any(|needle| lower.contains(needle));
    }
    false
}

/// Best-effort `(code, message)` extraction from a JSON-RPC-flavoured error
/// body, for feeding [`looks_like_depth_refusal`]. Tries the standard
/// envelope's `error` object first (`{"error":{"code":...,"message":...}}`),
/// then a bare `{"code":...,"message":...}` with no envelope at all — what
/// publicnode's `403` archive-depth response actually is, on the wire.
/// Returns `(None, String::new())` for anything that is not JSON or has
/// neither shape: this only ever feeds a heuristic, never changes whether
/// the call is reported as an error, so silently giving up is the right
/// failure mode rather than propagating a second error about the error.
fn extract_json_rpc_code_message(bytes: &[u8]) -> (Option<i64>, String) {
    let Ok(value) = serde_json::from_slice::<Value>(bytes) else {
        return (None, String::new());
    };
    let obj = value.get("error").unwrap_or(&value);
    let code = obj.get("code").and_then(Value::as_i64);
    let message = obj.get("message").and_then(Value::as_str).unwrap_or("").to_string();
    (code, message)
}

fn parse_err(method: &str, detail: &str) -> FeedError {
    FeedError::Parse {
        context: method.to_string(),
        detail: detail.to_string(),
    }
}

/// `eth_chainId` on `rpc`, checked against `expected`. Separated out
/// because it runs both at startup and lazily, before an endpoint that
/// could not be checked at startup is first trusted.
async fn verify_chain(rpc: &RpcClient, expected: u64) -> Result<(), FeedError> {
    let got = rpc.call("eth_chainId", json!([])).await?;
    let got = got
        .as_str()
        .ok_or_else(|| parse_err("eth_chainId", "result is not a string"))
        .and_then(|s| parse_hex_u128(s).map_err(|e| parse_err("eth_chainId", &e)))?;
    let got = u64::try_from(got).map_err(|_| parse_err("eth_chainId", "does not fit u64"))?;
    if got != expected {
        return Err(FeedError::ChainIdMismatch { expected, got });
    }
    Ok(())
}

/// Every configured endpoint declined the same call. Names each one and
/// its reason: with fallbacks configured, "the block did not load" is
/// almost always *one* provider's plan limit rather than the block being
/// unavailable, and the operator cannot tell which without the list.
fn all_failed(method: &str, failures: &[String]) -> FeedError {
    FeedError::Rpc {
        method: method.to_string(),
        detail: format!("all {} endpoint(s) failed — {}", failures.len(), failures.join(" | ")),
    }
}

/// `0x`-prefixed hex quantity → `u128`. Rejects empty digits, overflow,
/// and anything not `0x…` — never guesses.
fn parse_hex_u128(s: &str) -> Result<u128, String> {
    let digits = s
        .strip_prefix("0x")
        .or_else(|| s.strip_prefix("0X"))
        .ok_or_else(|| format!("expected 0x-hex quantity, got {s:?}"))?;
    if digits.is_empty() {
        return Err(format!("empty hex quantity: {s:?}"));
    }
    u128::from_str_radix(digits, 16).map_err(|_| format!("hex quantity does not fit u128: {s:?}"))
}

fn parse_address_str(s: &str) -> Option<Address> {
    parse_address(s)
}

/// `0x` + exactly 40 hex digits (either case).
fn parse_address(s: &str) -> Option<Address> {
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

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(byte: u8) -> Address {
        [byte; 20]
    }
    fn a_hex(byte: u8) -> String {
        format!("0x{}", hex(&a(byte)))
    }

    // ── endpoint list handling ─────────────────────────────────────────

    /// An empty endpoint list is a configuration error, caught before any
    /// I/O — `clients[0]` is indexed unconditionally elsewhere, so an
    /// empty `RpcFeed` must be unconstructible rather than a latent panic.
    #[tokio::test]
    async fn new_multi_rejects_an_empty_endpoint_list() {
        match RpcFeed::new_multi(vec![], 1).await {
            Err(FeedError::Internal(msg)) => {
                assert!(msg.contains("at least one"), "unhelpful message: {msg}");
            }
            Err(other) => panic!("expected Internal, got {other:?}"),
            Ok(_) => panic!("an empty endpoint list must not construct an RpcFeed"),
        }
    }

    /// The client must identify itself. Cloudflare-fronted public RPC
    /// endpoints reject a `User-Agent`-less POST with a 403 HTML page,
    /// and `reqwest` sends none unless told to — so an empty UA silently
    /// removes a fallback from service while `curl` checks keep passing.
    #[test]
    fn rpc_client_sends_a_user_agent() {
        assert!(USER_AGENT.starts_with("risepir-rpc/"), "unexpected UA: {USER_AGENT}");
        assert!(
            USER_AGENT.len() > "risepir-rpc/".len(),
            "UA must carry a version, got {USER_AGENT}"
        );
    }

    /// `all_failed` must name every endpoint that declined and its
    /// reason: with a fallback chain, "the block did not load" is usually
    /// one provider's plan limit, and the operator cannot tell which
    /// without the per-endpoint detail.
    #[test]
    fn all_failed_names_every_endpoint_and_reason() {
        let e = all_failed(
            "block_update(25613828)",
            &[
                "https://eth.drpc.org: HTTP 408 free plan".to_string(),
                "https://eth.merkle.io: HTTP 429".to_string(),
            ],
        );
        let FeedError::Rpc { method, detail } = e else { panic!("expected Rpc") };
        assert_eq!(method, "block_update(25613828)");
        assert!(detail.contains("all 2 endpoint(s) failed"), "{detail}");
        assert!(detail.contains("eth.drpc.org"), "{detail}");
        assert!(detail.contains("eth.merkle.io"), "{detail}");
        assert!(detail.contains("408"), "{detail}");
    }

    // ── changes_from_trace: the diffMode rules, one by one ─────────────

    #[test]
    fn post_balance_is_taken_pre_only_is_destroyed_no_balance_is_skipped() {
        let trace = serde_json::json!([
            {"txHash": "0x01", "result": {
                "pre": {
                    a_hex(0x11): {"balance": "0x64", "nonce": 1},        // balance changes
                    a_hex(0x22): {"balance": "0x0a"},                     // destroyed (absent from post)
                    a_hex(0x33): {"storage": {"0x0": "0x1"}},             // storage-only change
                },
                "post": {
                    a_hex(0x11): {"balance": "0xc8", "nonce": 2},
                    a_hex(0x33): {"storage": {"0x0": "0x2"}},             // no balance field -> skip
                    a_hex(0x44): {"balance": "0x01"},                     // created with funds
                }
            }}
        ]);
        let changes = changes_from_trace(&trace).unwrap();
        assert_eq!(
            changes,
            vec![(a(0x11), 0xc8), (a(0x22), 0), (a(0x44), 1)],
            "0x33's storage-only change must not fabricate a balance entry"
        );
    }

    #[test]
    fn later_tx_wins_across_the_block() {
        let trace = serde_json::json!([
            {"result": {"pre": {a_hex(0x11): {"balance": "0x64"}}, "post": {a_hex(0x11): {"balance": "0x65"}}}},
            {"result": {"pre": {a_hex(0x11): {"balance": "0x65"}}, "post": {a_hex(0x11): {"balance": "0x70"}}}},
        ]);
        assert_eq!(changes_from_trace(&trace).unwrap(), vec![(a(0x11), 0x70)]);
    }

    #[test]
    fn destroyed_then_recreated_later_tx_wins() {
        let trace = serde_json::json!([
            // tx0 destroys 0x11
            {"result": {"pre": {a_hex(0x11): {"balance": "0x64"}}, "post": {}}},
            // tx1 recreates it with 5 wei
            {"result": {"pre": {}, "post": {a_hex(0x11): {"balance": "0x05"}}}},
        ]);
        assert_eq!(changes_from_trace(&trace).unwrap(), vec![(a(0x11), 5)]);
    }

    #[test]
    fn nonce_only_change_after_balance_change_keeps_earlier_balance() {
        let trace = serde_json::json!([
            {"result": {"pre": {a_hex(0x11): {"balance": "0x64"}}, "post": {a_hex(0x11): {"balance": "0x99"}}}},
            // later tx touches 0x11's nonce only — balance stays 0x99
            {"result": {"pre": {a_hex(0x11): {"nonce": 5}}, "post": {a_hex(0x11): {"nonce": 6}}}},
        ]);
        assert_eq!(changes_from_trace(&trace).unwrap(), vec![(a(0x11), 0x99)]);
    }

    #[test]
    fn empty_block_yields_no_changes() {
        assert_eq!(changes_from_trace(&serde_json::json!([])).unwrap(), vec![]);
    }

    #[test]
    fn tracer_error_for_one_tx_fails_the_block() {
        let trace = serde_json::json!([
            {"result": {"pre": {}, "post": {}}},
            {"result": {"error": "execution timeout"}},
        ]);
        let err = changes_from_trace(&trace).unwrap_err();
        assert!(matches!(err, FeedError::Parse { .. }), "{err}");
    }

    #[test]
    fn malformed_balance_or_address_fails_loudly() {
        for bad in [
            serde_json::json!([{"result": {"pre": {}, "post": {a_hex(0x11): {"balance": "123"}}}}]), // no 0x
            serde_json::json!([{"result": {"pre": {}, "post": {a_hex(0x11): {"balance": 123}}}}]),   // not a string
            serde_json::json!([{"result": {"pre": {}, "post": {"0x1234": {"balance": "0x1"}}}}]),    // short addr
            serde_json::json!([{"result": {"post": {}}}]),                                            // missing pre
            serde_json::json!("not an array"),
        ] {
            assert!(changes_from_trace(&bad).is_err(), "accepted malformed trace: {bad}");
        }
    }

    // ── credits_from_block ──────────────────────────────────────────────

    #[test]
    fn withdrawals_convert_gwei_to_wei_and_keep_duplicates_in_order() {
        let block = serde_json::json!({
            "number": "0x1",
            "withdrawals": [
                {"address": a_hex(0x11), "amount": "0x2fadbb68"},
                {"address": a_hex(0x11), "amount": "0x1"},
                {"address": a_hex(0x22), "amount": "0x0"},
            ]
        });
        let credits = credits_from_block(&block).unwrap();
        assert_eq!(
            credits,
            vec![
                (a(0x11), 0x2fadbb68u128 * 1_000_000_000),
                (a(0x11), 1_000_000_000),
                (a(0x22), 0),
            ]
        );
    }

    #[test]
    fn pre_shanghai_block_without_withdrawals_is_empty() {
        assert_eq!(credits_from_block(&serde_json::json!({"number": "0x1"})).unwrap(), vec![]);
    }

    #[test]
    fn malformed_withdrawals_fail_loudly() {
        for bad in [
            serde_json::json!({"withdrawals": [{"address": a_hex(0x11)}]}),                    // no amount
            serde_json::json!({"withdrawals": [{"address": "0xzz", "amount": "0x1"}]}),        // bad addr
            serde_json::json!({"withdrawals": [{"address": a_hex(0x11), "amount": "1"}]}),     // no 0x
            serde_json::json!({"withdrawals": "nope"}),
        ] {
            assert!(credits_from_block(&bad).is_err(), "accepted malformed block: {bad}");
        }
    }

    // ── hex parsing ─────────────────────────────────────────────────────

    #[test]
    fn hex_u128_rejects_garbage_and_accepts_bounds() {
        assert_eq!(parse_hex_u128("0x0").unwrap(), 0);
        assert_eq!(parse_hex_u128("0xffffffffffffffffffffffffffffffff").unwrap(), u128::MAX);
        for bad in ["", "0x", "12", "0xg", "0x100000000000000000000000000000000"] {
            assert!(parse_hex_u128(bad).is_err(), "accepted {bad:?}");
        }
    }

    // ── depth-refusal classification (ADR-0036) ─────────────────────────

    /// The exact publicnode wire response this classifier exists for:
    /// `HTTP 403 {"code":-32602,"message":"Archive requests require a
    /// personal token..."}`. Status alone is sufficient, and the embedded
    /// code/message (once extracted) agree independently.
    #[test]
    fn publicnode_archive_refusal_classifies_as_depth_refusal() {
        let body = br#"{"code":-32602,"message":"Archive requests require a personal token..."}"#;
        let (code, message) = extract_json_rpc_code_message(body);
        assert_eq!(code, Some(-32602));
        assert!(message.contains("Archive requests require a personal token"));
        assert!(looks_like_depth_refusal(403, code, &message));
        // The HTTP status alone is sufficient -- even a body that failed to
        // parse at all must still classify as a depth refusal on a 403.
        assert!(looks_like_depth_refusal(403, None, ""));
    }

    /// A full node's classic "missing trie node" / "pruned" errors ride a
    /// `200` with a JSON-RPC `error` member in the wild (Geth et al.), not a
    /// `403` -- the code+message branch must catch these too.
    #[test]
    fn full_node_pruning_errors_classify_without_a_403() {
        assert!(looks_like_depth_refusal(200, Some(-32000), "missing trie node abcd1234 (archive node needed?)"));
        assert!(looks_like_depth_refusal(200, Some(-32000), "PRUNED: state not retained for this block"));
        assert!(looks_like_depth_refusal(200, Some(-32602), "state is not available for the requested block"));
    }

    /// A transport failure never reaches the classifier at all (no HTTP
    /// response exists to classify) -- it is built directly as a plain
    /// `Rpc`, and must never be misreported as a depth refusal.
    #[test]
    fn transport_error_is_not_a_depth_refusal() {
        let err = FeedError::Rpc {
            method: "eth_getBalance".to_string(),
            detail: "transport: error sending request".to_string(),
        };
        assert!(!err.is_depth_refusal());
    }

    /// A rate limit must not classify as a depth refusal, whether it
    /// surfaces as a `429` or as a `200` with an unrelated JSON-RPC code --
    /// a different failure mode with a different fix (backoff, not "stop
    /// asking this depth").
    #[test]
    fn rate_limit_is_not_a_depth_refusal() {
        assert!(!looks_like_depth_refusal(429, None, "Too Many Requests"));
        assert!(!looks_like_depth_refusal(429, Some(-32005), "limit exceeded, please retry later"));
        assert!(!looks_like_depth_refusal(200, Some(-32005), "rate limit exceeded"));
    }

    /// The right message with the wrong code, or no code at all, must not
    /// classify -- the rule is a conjunction, not just a keyword search.
    #[test]
    fn keyword_without_the_right_code_does_not_classify() {
        assert!(!looks_like_depth_refusal(200, Some(-32601), "archive node required"));
        assert!(!looks_like_depth_refusal(200, None, "archive node required"));
    }

    /// The right code with an unrelated message must not classify either --
    /// `-32000` alone covers a lot of ordinary JSON-RPC errors (e.g. a
    /// reverted call), not just depth refusals.
    #[test]
    fn right_code_without_a_keyword_does_not_classify() {
        assert!(!looks_like_depth_refusal(200, Some(-32000), "execution reverted"));
    }

    /// `extract_json_rpc_code_message` must also read the standard envelope
    /// shape (`{"error":{"code":...,"message":...}}`), not just the bare
    /// shape publicnode's `403` uses.
    #[test]
    fn extract_reads_the_standard_envelope_shape_too() {
        let body = br#"{"jsonrpc":"2.0","id":1,"error":{"code":-32000,"message":"missing trie node xyz"}}"#;
        let (code, message) = extract_json_rpc_code_message(body);
        assert_eq!(code, Some(-32000));
        assert!(message.contains("missing trie node"));
    }

    /// Malformed/non-JSON bodies degrade to "nothing extracted" rather than
    /// panicking or erroring -- this only ever feeds a heuristic.
    #[test]
    fn extract_degrades_quietly_on_non_json() {
        assert_eq!(extract_json_rpc_code_message(b"not json at all"), (None, String::new()));
    }
}

/// Live-network tests: run explicitly with
/// `cargo test -p risepir-feed --release -- --ignored`. They talk to
/// public keyless endpoints (dRPC for traces, publicnode for the
/// independent balance check) and assert this module's diffMode reading
/// of a real finalized block reproduces a second provider's
/// `eth_getBalance` exactly — the strongest offline-unavailable evidence
/// that the parsing rules above are the chain's own semantics.
#[cfg(test)]
mod live_tests {
    use super::*;

    const FEED_URL: &str = "https://eth.drpc.org";
    const CONFIRM_URL: &str = "https://ethereum-rpc.publicnode.com";

    #[tokio::test]
    #[ignore = "live network: dRPC + publicnode"]
    async fn live_block_update_matches_independent_provider() {
        let feed = RpcFeed::new(FEED_URL, 1).await.expect("connect + chain id 1");
        let confirm = RpcClient::new(CONFIRM_URL);

        let n = feed.finalized().await.expect("finalized");
        let FetchedBlock { update, changed: raw_changes, .. } = feed.block_update(n).await.expect("block_update");
        assert_eq!(update.block, n);
        assert!(
            !update.changes.is_empty(),
            "a mainnet block with zero balance changes does not exist (gas alone moves the fee recipient)"
        );

        // The load-bearing check: our post-block balances, recomputed from
        // the trace, must equal what an INDEPENDENT provider says the
        // balance was at exactly block n. Withdrawal recipients are
        // excluded — their post-block balance additionally includes
        // credits the trace cannot see (that is ADR-0018's whole point).
        let withdrawal_recipients: std::collections::HashSet<Address> = {
            let block = feed
                .rpc()
                .call("eth_getBlockByNumber", json!([format!("0x{n:x}"), false]))
                .await
                .expect("block body");
            credits_from_block(&block)
                .expect("credits")
                .into_iter()
                .map(|(a, _)| a)
                .collect()
        };

        let mut checked = 0usize;
        for (addr, our_balance) in raw_changes.iter().take(24) {
            if withdrawal_recipients.contains(addr) {
                continue;
            }
            let reference = confirm.balance_at(addr, n).await.expect("confirm balance_at");
            assert_eq!(
                *our_balance,
                reference,
                "trace-derived balance for 0x{} at block {n} disagrees with {CONFIRM_URL}",
                hex(addr)
            );
            checked += 1;
        }
        assert!(checked >= 5, "too few non-withdrawal addresses sampled ({checked}) to be meaningful");
        println!("live conformance: {checked} addresses byte-exact vs independent provider at block {n}");
    }

    #[tokio::test]
    #[ignore = "live network: dRPC"]
    async fn live_chain_id_mismatch_rejected() {
        let err = match RpcFeed::new(FEED_URL, 11155111).await {
            Ok(_) => panic!("a mainnet endpoint must be rejected when Sepolia's chain id is expected"),
            Err(e) => e,
        };
        assert!(matches!(err, FeedError::ChainIdMismatch { expected: 11155111, got: 1 }), "{err}");
    }
}
