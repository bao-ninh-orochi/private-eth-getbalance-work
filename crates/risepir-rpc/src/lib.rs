#![forbid(unsafe_code)]
#![warn(missing_docs)]
//! Stage 0.4: a standard Ethereum JSON-RPC front end on `:8545`
//! (`docs/plan.md` §3, §6) over the RisePIR HTTP transport
//! ([`risepir_http`]) and the response-rewind client ([`risepir_client`]).
//!
//! # What this crate is
//!
//! The seam between "the private PIR pipeline" (everything below this
//! crate: `risepir-proto`/`risepir-server`/`risepir-client`/`risepir-http`)
//! and "a wallet or `cast` pointed at `localhost:8545`". [`PrivateEth`] is
//! the core: one [`risepir_client::RisePirClient`] behind a `Mutex`
//! (`docs/plan.md` ADR-0010), driving one [`risepir_http::PirHttpClient`].
//! [`rpc`] is the JSON-RPC 2.0 surface on top, deny-by-default for every
//! method except the four this deployment can answer honestly without a
//! trusted third party ever learning which account was asked about
//! (`docs/plan.md` ADR-0012).
//!
//! # Deny-by-default (ADR-0012)
//!
//! `eth_getBalance` is answered privately. `eth_chainId` / `net_version` /
//! `eth_blockNumber` are answered from local deployment state (no leak —
//! they carry no address). Every other method is refused (`-32601`) unless
//! an operator explicitly opts into `--proxy-upstream <url>`, in which
//! case it is forwarded verbatim to that upstream — a conscious trade
//! (wallet compatibility) taken loudly, never the silent default.
//!
//! # `"latest"` means *our* head (ADR-0007)
//!
//! This deployment follows `finalized`, not the chain's own `"latest"` —
//! so this service's own head can lag a public RPC's by the finalization
//! window. `eth_getBalance`'s block parameter accepts a tag
//! (`"latest"`/`"pending"`/`"safe"`/`"finalized"`, all synonyms for "our
//! head" here) or a hex block number **equal to** our head; anything else
//! is refused rather than silently answered against the wrong epoch.
//!
//! # Scope
//!
//! [`demo`] provides a self-contained mock-backed deployment (genesis
//! population, a background block-follow loop, a handful of known demo
//! accounts) that both `src/main.rs` and this crate's own integration test
//! drive — Stage 0.4 is deliberately mock-backed throughout; wiring real
//! mainnet data is Stage 1 (`docs/plan.md` §6).

// MUST stay first: `macro_rules!` macros are textually scoped, so
// `#[macro_use]` only makes `logln!` visible to modules declared *after*
// this line. Moving it below any of them breaks their build with
// "cannot find macro `logln` in this scope".
#[macro_use]
pub mod logging;

mod error;
mod private_eth;
pub mod rpc;

pub mod autosave;
pub mod demo;
pub mod front;
pub mod hard_refresh;
pub mod journal;
pub mod mainnet;
pub mod probe;
pub mod snapshot_audit;
pub mod snapshot_rewind;
pub mod state;

pub use error::RpcError;
pub use private_eth::{BalanceTimings, PrivateEth, SyncTimings};
pub use risepir_proto::keccak256;
