//! A small, self-contained mock-backed deployment: a [`risepir_feed::MockFeed`]
//! chain, a [`risepir_server::RisePirServer`] built from its genesis
//! snapshot plus a handful of known demo accounts, a background
//! block-follow loop, and the JSON-RPC front end on top — everything
//! [`crate`]'s `main.rs` binary needs, factored out here so the Part C
//! integration test can stand up the *exact same* stack in-process rather
//! than a hand-duplicated approximation of it.
//!
//! # Why demo accounts exist at all
//!
//! [`risepir_feed::MockFeed`]'s genesis/churn population is keyed by
//! addresses only the feed itself knows how to generate
//! ([`risepir_feed::MockFeed::address_for`]) — there is no way for an
//! outside caller (a human running `cast balance`, or the Part C test) to
//! name one of those addresses without reaching into the feed's internals.
//! So this module additionally seeds a short, fixed list of ordinary
//! 20-byte addresses with known round wei balances, inserted the same way
//! any real balance would be (`keccak256(address)` as the key,
//! `ValueCodec::encode` as the value) — these are exactly what
//! `cast balance <addr>` can query and get a predictable answer from. The
//! churning mock population is not otherwise queryable by a 20-byte
//! address; it exists purely as background load that exercises the delta
//! stream and the rewind, which is exactly what it is for.

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::atomic::AtomicU64;
use std::sync::Arc;
use std::time::Duration;

use ikpir_common::{SimpleConfig, SimplePirBackend};
use risepir_client::RisePirClient;
use risepir_feed::{Feed, MockConfig, MockFeed};
use risepir_http::{NodeState, PirHttpClient};
use risepir_proto::{Backend, Geometry, ValueCodec};
use risepir_server::{DeltaRing, RisePirServer};
use segmented_cuckoo::{Segmented3aryCuckooKVStore, Segmented3aryScheme};

use risepir_proto::keccak256;
use crate::private_eth::PrivateEth;

/// SCF geometry constants for the demo deployment. `LWE_DIM` is well
/// below `SimpleConfig::default()`'s 1275: this deployment is a few
/// thousand accounts (`DemoConfig::num_genesis_keys`), so the reshaped
/// per-segment matrices are already small, and a smaller `lwe_dim` keeps
/// every `/answer` call (and the background follow loop's per-block hint
/// patch) fast enough for an interactive demo — the same trade-off
/// `risepir-http`'s own test suite makes (`tests/http.rs`'s `LWE_DIM`).
/// Security is not the point of Stage 0.4's demo; wiring correctness is.
const ARITY: u32 = 3;
const BUCKET_SIZE: u32 = 4;
const FINGERPRINT_BITS: u32 = 32;
const LWE_DIM: u32 = 512;

fn value_codec() -> ValueCodec {
    ValueCodec {
        key_tag_bits: 32,
        balance_bits: 96,
        checksum_bits: 16,
    }
}

/// Knobs for [`spawn`]. [`Default`] matches this stage's brief exactly
/// (`num_genesis_keys` ~5000, ~40 changes/block, 10 inserts/10 deletes);
/// the port/interval fields exist so the Part C integration test can
/// override them (ephemeral ports, a much faster follow cadence) without
/// duplicating the rest of the deployment logic.
#[derive(Clone, Debug)]
pub struct DemoConfig {
    /// JSON-RPC listen port. `0` binds an OS-assigned ephemeral port.
    pub rpc_port: u16,
    /// PIR HTTP listen port. `0` binds an OS-assigned ephemeral port.
    pub pir_port: u16,
    /// `eth_chainId` / `net_version` value.
    pub chain_id: u64,
    /// Opt-in proxy target for denied methods (`docs/plan.md` ADR-0012).
    pub proxy_upstream: Option<String>,
    /// [`MockConfig::seed`].
    pub seed: u64,
    /// [`MockConfig::num_genesis_keys`].
    pub num_genesis_keys: u64,
    /// [`MockConfig::changes_per_block`].
    pub changes_per_block: usize,
    /// [`MockConfig::inserts_per_block`].
    pub inserts_per_block: usize,
    /// [`MockConfig::deletes_per_block`].
    pub deletes_per_block: usize,
    /// How often the background follow loop pulls one block from the
    /// feed and applies it. The brief calls for "fast cadence for the
    /// demo, not 12s" — `main.rs` uses the `Default` below (~1s); the
    /// Part C test uses a much shorter interval so the suite stays fast.
    pub block_interval: Duration,
    /// [`DeltaRing::new`]'s capacity, in blocks.
    pub ring_capacity: usize,
}

impl Default for DemoConfig {
    fn default() -> Self {
        Self {
            rpc_port: 8545,
            pir_port: 8645,
            chain_id: 1,
            proxy_upstream: None,
            seed: 0x5249_5345_5049_5232, // "RISEPIR2" in ASCII — fixed, so the demo is reproducible run to run
            num_genesis_keys: 5_000,
            changes_per_block: 40,
            inserts_per_block: 10,
            deletes_per_block: 10,
            block_interval: Duration::from_secs(1),
            ring_capacity: 600,
        }
    }
}

/// What [`spawn`] hands back: where everything ended up listening, and the
/// exact demo accounts it seeded (so a caller — `main.rs`, or the Part C
/// test — never has to hardcode the list a second time).
pub struct DemoHandle {
    /// Bound address of the JSON-RPC front end.
    pub rpc_addr: SocketAddr,
    /// Bound address of the PIR HTTP transport.
    pub pir_addr: SocketAddr,
    /// `(address, balance)` pairs seeded at startup — see the module docs.
    pub demo_accounts: Vec<([u8; 20], u128)>,
}

/// The fixed list of known demo accounts (module docs explain why these
/// exist). Deterministic, round-number addresses and wei-exact — not
/// ETH-rounded only — balances, so a caller can eyeball both the address
/// and the expected `cast balance` output at a glance.
fn demo_accounts() -> Vec<([u8; 20], u128)> {
    const ETH: u128 = 1_000_000_000_000_000_000;
    vec![
        ([0x11u8; 20], ETH),           // 1 ETH
        ([0x22u8; 20], 5 * ETH),       // 5 ETH
        ([0x33u8; 20], 42 * ETH),      // 42 ETH
        ([0x44u8; 20], 1_000 * ETH),   // 1000 ETH
        ([0x55u8; 20], 100),           // 100 wei — a sub-gwei "dust" balance, proving this is wei-exact
    ]
}

/// Build and spawn the whole demo stack: the mock chain, the PIR server,
/// the background follow loop, and the JSON-RPC front end — everything
/// running as detached `tokio` tasks by the time this returns.
///
/// # Panics
///
/// On any construction failure (bad geometry, a port already in use,
/// the freshly-started PIR server not answering `/setup`, ...). Every one
/// of these is a deployment-time configuration or environment problem —
/// exactly the class of failure `risepir_server::RisePirServer::new`'s own
/// docs argue should panic loudly at construction rather than surface as
/// a `Result` a caller might paper over — not a condition that can arise
/// from serving live traffic (nothing here runs after startup completes).
pub async fn spawn(cfg: DemoConfig) -> DemoHandle {
    let value_codec = value_codec();

    let mock_cfg = MockConfig {
        seed: cfg.seed,
        num_genesis_keys: cfg.num_genesis_keys,
        changes_per_block: cfg.changes_per_block,
        inserts_per_block: cfg.inserts_per_block,
        deletes_per_block: cfg.deletes_per_block,
    };
    let mut feed = MockFeed::new(mock_cfg);

    let geom = Geometry::for_accounts(cfg.num_genesis_keys, ARITY, BUCKET_SIZE, FINGERPRINT_BITS, &value_codec, Backend::Simple)
        .expect("demo geometry");
    let mut store = Segmented3aryCuckooKVStore::new(
        geom.num_buckets,
        geom.bucket_size,
        geom.fingerprint_bits,
        geom.value_bits,
        geom.plaintext_bits,
    )
    .expect("demo store");

    for (addr, bal) in feed.snapshot() {
        let v = value_codec.encode(&addr, bal).expect("encode genesis balance");
        store.insert(addr, &v).expect("insert genesis balance");
    }

    let demo_accounts = demo_accounts();
    for (addr20, balance) in &demo_accounts {
        let key = keccak256(addr20);
        let v = value_codec.encode(&key, *balance).expect("encode demo account");
        store.insert(key, &v).expect("insert demo account");
    }

    let server: RisePirServer<Segmented3aryScheme, SimplePirBackend> =
        RisePirServer::new(store, SimpleConfig::with_lwe_dim(LWE_DIM), value_codec, 0);
    let node_state = Arc::new(NodeState::new(server, DeltaRing::new(cfg.ring_capacity)));

    let pir_listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, cfg.pir_port))
        .await
        .expect("bind PIR HTTP port");
    let pir_addr = pir_listener.local_addr().expect("PIR local_addr");
    let pir_router = NodeState::router(node_state.clone());
    tokio::spawn(async move {
        axum::serve(pir_listener, pir_router).await.expect("PIR HTTP server crashed");
    });

    // Background follow loop: every `cfg.block_interval`, pull one block
    // from the feed and apply it — the "epoch = block" cadence
    // `docs/plan.md` §3.4 describes, at demo speed rather than mainnet's
    // 12s.
    {
        let node_state = node_state.clone();
        let interval = cfg.block_interval;
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(interval).await;
                match feed.next_block() {
                    Ok(Some(update)) => {
                        if let Err(e) = node_state.apply_block(&update).await {
                            eprintln!("risepir-rpc: follow loop: apply_block failed: {e}");
                        }
                    }
                    Ok(None) => {} // MockFeed never returns None; kept for a future real feed
                    Err(e) => eprintln!("risepir-rpc: follow loop: feed error: {e}"),
                }
            }
        });
    }

    let pir_client = PirHttpClient::new(format!("http://{pir_addr}"));
    let setup_bundle = pir_client.setup().await.expect("GET /setup against the freshly-started PIR server");
    let arity = setup_bundle.params.arity();
    let plaintext_bits = setup_bundle.params.plaintext_bits;
    let reshape_row_width_per_seg: Vec<u32> = setup_bundle.backend_params.iter().map(|sp| sp.reshape_row_width).collect();
    let pinned_block = setup_bundle.block;

    let rise_client: RisePirClient<SimplePirBackend> = RisePirClient::from_setup(setup_bundle, value_codec);

    let private_eth = Arc::new(PrivateEth {
        client: tokio::sync::Mutex::new(rise_client),
        pending_head: AtomicU64::new(pinned_block),
        pir: pir_client,
        reshape_row_width_per_seg,
        arity,
        plaintext_bits,
        chain_id: cfg.chain_id,
        strict_not_found: false, // the mock's account universe is complete by construction
        proxy_upstream: cfg.proxy_upstream,
        proxy_http: reqwest::Client::new(),
    });

    let rpc_listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, cfg.rpc_port))
        .await
        .expect("bind JSON-RPC port");
    let rpc_addr = rpc_listener.local_addr().expect("RPC local_addr");
    let rpc_router = crate::rpc::router(private_eth);
    tokio::spawn(async move {
        axum::serve(rpc_listener, rpc_router).await.expect("JSON-RPC server crashed");
    });

    DemoHandle { rpc_addr, pir_addr, demo_accounts }
}
