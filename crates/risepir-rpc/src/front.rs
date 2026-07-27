//! `risepir-rpc client` — the **remote front end**: the JSON-RPC surface
//! plus the response-rewind PIR client, running on *your* machine against
//! a PIR server somewhere else (`docs/deploy.md`, remote deployment).
//!
//! # Why this exists
//!
//! In the single-process `mock`/`mainnet` deployments the rewind client
//! lives next to the server, so "the server never learns the address" is
//! true of the PIR math but both roles share one box. This mode splits
//! them for real: the server machine (EC2, a VPS) runs only the PIR
//! transport (`:8645` — LWE queries in, LWE responses and public deltas
//! out), and this process — on the wallet's own machine — holds the hint,
//! builds the queries, rewinds the responses, and serves plain Ethereum
//! JSON-RPC on localhost for `cast`/MetaMask. What crosses the network is
//! exactly what the PIR privacy claim covers; the address never leaves
//! this machine in any form.
//!
//! # Bootstrap cost
//!
//! `GET /setup` downloads the public matrix seed material and every
//! segment's hint once — `docs/numbers.md` §4c: ~95 MB at 1 M accounts,
//! ~267 MB at 9.4 M, a few GB at the complete mainnet set. That is the
//! inherent SimplePIR-family client footprint, paid once per deployment
//! (restarts of the *server* keep `A`/hints bit-identical via its state
//! file, so this client's bundle stays valid across them).
//!
//! # The `NotFound` policy is fetched, never guessed
//!
//! `GET /mode` tells this front end whether the server holds the complete
//! nonzero set (absence ⇒ `0x0`, ADR-0015) or a partial one (absence ⇒
//! error, never `0x0`). Guessing this flag would be a silent-wrong-answer
//! bug, so there is no CLI override for it.

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;

use risepir_http::PirHttpClient;

use crate::private_eth::PrivateEth;

/// Knobs for [`spawn`].
#[derive(Clone, Debug)]
pub struct FrontConfig {
    /// Base URL of the remote PIR transport, e.g. `http://1.2.3.4:8645`.
    pub pir_url: String,
    /// Local JSON-RPC listen port.
    pub rpc_port: u16,
    /// Local bind address (loopback unless you mean to share it onward).
    pub bind: Ipv4Addr,
    /// `eth_chainId` / `net_version` value (1 for mainnet; the PIR
    /// transport does not carry a chain id, so it is configured here).
    pub chain_id: u64,
    /// Opt-in proxy for denied methods (ADR-0012) — leaks exactly what it
    /// says on the warning.
    pub proxy_upstream: Option<String>,
}

impl Default for FrontConfig {
    fn default() -> Self {
        Self {
            pir_url: String::new(),
            rpc_port: 8545,
            bind: Ipv4Addr::LOCALHOST,
            chain_id: 1,
            proxy_upstream: None,
        }
    }
}

/// What [`spawn`] reports back for the startup banner.
pub struct FrontHandle {
    /// Bound local JSON-RPC address.
    pub rpc_addr: SocketAddr,
    /// Whether the remote deployment serves a complete set (`GET /mode`).
    pub complete: bool,
    /// The block the downloaded hint is pinned at.
    pub pinned_block: u64,
}

fn die(msg: impl std::fmt::Display) -> ! {
    eprintln!("risepir-rpc client: fatal: {msg}");
    std::process::exit(1);
}

/// Connect to the remote PIR transport, download the setup bundle, learn
/// the deployment mode, and serve JSON-RPC locally. Returns once the
/// listener is up.
pub async fn spawn(cfg: FrontConfig) -> FrontHandle {
    if cfg.pir_url.is_empty() {
        die("--pir-url is required (e.g. --pir-url http://<server>:8645)");
    }
    let codec = crate::mainnet::value_codec();
    let pir_client = PirHttpClient::new(cfg.pir_url.clone());

    eprintln!(
        "risepir-rpc client: downloading setup bundle from {} (A + hints; ~100 MB per 1M accounts) ...",
        cfg.pir_url
    );
    let started = std::time::Instant::now();
    // Mode and bundle from one atomic `/setup` response where the server
    // provides `x-risepir-mode` (ADR-0033); a separate `GET /mode` only as
    // the fallback for servers predating the header. See
    // `PirHttpClient::setup_with_mode` for why the pair must not straddle
    // two requests.
    let (setup_bundle, header_mode) = match pir_client.setup_with_mode().await {
        Ok(pair) => pair,
        Err(e) => die(format!("GET /setup from {}: {e}", cfg.pir_url)),
    };
    let complete = match header_mode {
        Some(m) => m,
        None => match pir_client.mode().await {
            Ok(m) => m,
            Err(e) => die(format!("GET /mode from {}: {e}", cfg.pir_url)),
        },
    };
    let pinned_block = setup_bundle.block;
    eprintln!(
        "risepir-rpc client: setup downloaded in {:.1}s — hint pinned at block {pinned_block}",
        started.elapsed().as_secs_f64()
    );

    let private_eth = Arc::new(PrivateEth::from_setup(
        pir_client,
        setup_bundle,
        codec,
        complete,
        cfg.chain_id,
        cfg.proxy_upstream.clone(),
    ));

    let rpc_listener = tokio::net::TcpListener::bind((cfg.bind, cfg.rpc_port))
        .await
        .unwrap_or_else(|e| die(format!("bind JSON-RPC port {}: {e}", cfg.rpc_port)));
    let rpc_addr = rpc_listener.local_addr().expect("RPC local_addr");
    tokio::spawn({
        let router = crate::rpc::router(private_eth);
        async move {
            if let Err(e) = axum::serve(rpc_listener, router).await {
                // A dead listener with a live process is a silent outage;
                // die loudly so the wallet sees a refused connection, not
                // an indefinite hang against a half-alive front end.
                eprintln!("risepir-rpc client: fatal: JSON-RPC listener crashed: {e}");
                std::process::exit(1);
            }
        }
    });

    FrontHandle {
        rpc_addr,
        complete,
        pinned_block,
    }
}

/// URL for reaching a listener this same process just bound: loopback
/// when the bind address is unspecified (`0.0.0.0` accepts connections
/// but is not a reliable *destination*), the bind address itself
/// otherwise.
pub(crate) fn local_url(bind: Ipv4Addr, port: u16) -> String {
    let ip = if bind.is_unspecified() { Ipv4Addr::LOCALHOST } else { bind };
    format!("http://{ip}:{port}")
}
