//! Stage 0.4 Part C: end-to-end integration test. Stands up the whole
//! stack in-process on ephemeral ports — the mock chain, the PIR HTTP
//! server, the background block-follow loop, and the JSON-RPC front end,
//! all via [`risepir_rpc::demo::spawn`] (the exact same code path
//! `src/main.rs` runs) — lets a few blocks apply, then drives it with real
//! JSON-RPC requests over real HTTP (not `tower::oneshot`): the same path
//! an external tool like `cast` exercises.

use std::time::Duration;

use serde_json::{json, Value};

use risepir_rpc::demo::{self, DemoConfig};

/// A deterministic, distinctive chain id (never Ethereum mainnet's `1`,
/// so a passing `eth_chainId`/`net_version` assertion actually proves this
/// test's own `--chain-id`-equivalent config plumbing works, rather than
/// coincidentally matching some hardcoded default elsewhere).
const TEST_CHAIN_ID: u64 = 1337;

fn test_config() -> DemoConfig {
    DemoConfig {
        rpc_port: 0, // OS-assigned ephemeral port
        pir_port: 0, // OS-assigned ephemeral port
        chain_id: TEST_CHAIN_ID,
        proxy_upstream: None,
        // A fast follow cadence keeps this test quick; deployment
        // geometry (num_genesis_keys / changes_per_block / seed / ring
        // capacity) stays at `DemoConfig::default()`'s Stage-0.4-brief
        // values, so this test exercises the exact same deployment shape
        // `main.rs` runs, not a scaled-down approximation of it.
        block_interval: Duration::from_millis(15),
        ..DemoConfig::default()
    }
}

/// POSTs one JSON-RPC 2.0 request to `base` and returns the parsed
/// response body.
async fn rpc(http: &reqwest::Client, base: &str, method: &str, params: Value) -> Value {
    let body = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
    let resp = http
        .post(base)
        .json(&body)
        .send()
        .await
        .expect("HTTP request failed");
    assert_eq!(
        resp.status(),
        200,
        "every JSON-RPC round trip must be HTTP 200 (errors live in the body)"
    );
    resp.json::<Value>()
        .await
        .expect("response was not valid JSON")
}

/// Polls `eth_blockNumber` until it reports at least `target` (parsed from
/// its `0x`-hex result), or panics after a generous timeout — deterministic
/// synchronization with the background follow loop instead of a blind sleep.
async fn wait_for_block(http: &reqwest::Client, base: &str, target: u64) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let resp = rpc(http, base, "eth_blockNumber", json!([])).await;
        let hex = resp["result"]
            .as_str()
            .expect("eth_blockNumber result must be a string");
        let head = u64::from_str_radix(hex.trim_start_matches("0x"), 16)
            .expect("eth_blockNumber result must be 0x-hex");
        if head >= target {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for block {target}; last observed head was {head}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn end_to_end_json_rpc() {
    let handle = demo::spawn(test_config()).await;
    let base = format!("http://{}/", handle.rpc_addr);
    let http = reqwest::Client::new();

    // Let a few blocks apply before querying, so the private-lookup path
    // (which syncs the client's pending delta forward against the PIR
    // server's actual head) is meaningfully exercised, not just answering
    // against genesis.
    wait_for_block(&http, &base, 3).await;

    // ── eth_getBalance: every seeded demo account returns its known
    // balance, as a minimal-digit 0x-hex quantity. ──────────────────────
    for (addr, balance) in &handle.demo_accounts {
        let addr_hex = format!(
            "0x{}",
            addr.iter().map(|b| format!("{b:02x}")).collect::<String>()
        );
        let resp = rpc(&http, &base, "eth_getBalance", json!([addr_hex, "latest"])).await;
        let expected = format!("0x{balance:x}");
        assert_eq!(
            resp["result"],
            json!(expected),
            "demo account {addr_hex} must return its known balance; full response: {resp}"
        );
        assert!(
            resp.get("error").is_none(),
            "a successful call must not carry an error object: {resp}"
        );
        assert_eq!(resp["jsonrpc"], json!("2.0"));
        assert_eq!(resp["id"], json!(1));
    }

    // ── eth_getBalance: a never-seeded random address returns "0x0" —
    // never an error, never a nonzero value (ADR-0015: absence == zero). ──
    let random_addr = "0xdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef";
    let resp = rpc(
        &http,
        &base,
        "eth_getBalance",
        json!([random_addr, "latest"]),
    )
    .await;
    assert_eq!(
        resp["result"],
        json!("0x0"),
        "a never-seeded address must return 0x0; full response: {resp}"
    );

    // ── eth_chainId / net_version / eth_blockNumber: sane, and reflect
    // this deployment's actual configured chain id. ─────────────────────
    let chain_id_resp = rpc(&http, &base, "eth_chainId", json!([])).await;
    assert_eq!(
        chain_id_resp["result"],
        json!(format!("0x{TEST_CHAIN_ID:x}"))
    );

    let net_version_resp = rpc(&http, &base, "net_version", json!([])).await;
    assert_eq!(net_version_resp["result"], json!(TEST_CHAIN_ID.to_string()));

    let block_number_resp = rpc(&http, &base, "eth_blockNumber", json!([])).await;
    let head_hex = block_number_resp["result"]
        .as_str()
        .expect("eth_blockNumber result must be a string");
    assert!(
        head_hex.starts_with("0x"),
        "eth_blockNumber must be 0x-prefixed: {head_hex}"
    );
    let head = u64::from_str_radix(&head_hex[2..], 16).expect("eth_blockNumber must be valid hex");
    assert!(
        head >= 3,
        "head must have advanced at least as far as wait_for_block already confirmed, got {head}"
    );

    // ── An unknown method with no proxy configured must be denied,
    // never silently ignored and never forwarded anywhere. ──────────────
    let denied_resp = rpc(
        &http,
        &base,
        "eth_sendTransaction",
        json!([{"from": random_addr}]),
    )
    .await;
    assert!(
        denied_resp.get("result").is_none(),
        "a denied method must never carry a result: {denied_resp}"
    );
    assert_eq!(
        denied_resp["error"]["code"],
        json!(-32601),
        "denied method must be JSON-RPC -32601: {denied_resp}"
    );
    let message = denied_resp["error"]["message"]
        .as_str()
        .expect("error message must be a string");
    assert!(
        message.contains("denied by default"),
        "denial message should explain the deny-by-default posture, got: {message}"
    );

    // ── Malformed JSON must never panic / 500 — always a clean JSON-RPC
    // error, HTTP 200. ────────────────────────────────────────────────
    let raw = http
        .post(&base)
        .header("content-type", "application/json")
        .body("{ this is not valid json")
        .send()
        .await
        .expect("HTTP request failed");
    assert_eq!(
        raw.status(),
        200,
        "even a parse failure must be a clean 200-wrapped JSON-RPC error, not 500"
    );
    let malformed_body: Value = raw
        .json()
        .await
        .expect("even the error response must be valid JSON");
    assert_eq!(malformed_body["error"]["code"], json!(-32700));
}
