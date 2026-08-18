#![forbid(unsafe_code)]
#![warn(missing_docs)]
//! The response-rewind RisePIR client (see `docs/plan.md` §3.3, ADR-0003):
//! the core novel mechanism of the private `eth_getBalance` service.
//!
//! # The rewind, in one picture
//!
//! A client pins `(A, H₀, block₀)` at [`RisePirClient::from_setup`] and
//! never has to catch up to the server's head. A query answered at the
//! server's current block `E'` is corrected, using only public
//! information the client already has, into *exactly* the response a
//! server pinned at `block₀` would have given:
//!
//! ```text
//! 1. resp  ← server.answer(q)              // answered at the server's head E'
//! 2. resp -= qᵀ·ΔD[block₀ → E']            // → BIT-EXACT the response a block₀ server would give
//! 3. cells ← B::client_decode(state, resp) // → the bucket AS OF block₀   (decodes vs the STALE hint)
//! 4. cells += ΔD[row]                      // → the bucket AS OF E'    ***BEFORE STEP 5***
//! 5. scan cells for fp(key)
//! ```
//!
//! **Step 4 must precede step 5.** If a key was inserted (or kicked into
//! a different slot) after `block₀`, the pinned bucket alone has no
//! matching fingerprint; scanning first silently reports "not found" for
//! an account that demonstrably exists — the silent-wrong-answer bug
//! `docs/verification.md` reproduces and this crate regression-tests
//! (`created_during_run`, `ordering_trap_is_real`). [`RisePirClient::finish`]
//! performs steps 2 through 5 in exactly this order; it is not
//! configurable.
//!
//! # Why not `ikpir_client::IkpirClient`
//!
//! `IkpirClient::decode` runs the fingerprint scan *internally*, after
//! its own hint-patch bookkeeping, and returns a *value* rather than raw
//! cells; `IkpirClient::apply_delta` only ever accepts a delta at exactly
//! `epoch + 1`. This design needs the **raw bucket cells** so the delta
//! correction can be applied *before* the scan (step 4 above), and needs
//! to apply an **arbitrarily coalesced** delta (any number of blocks at
//! once) rather than one epoch at a time. Both needs are met by driving
//! the `IndexPirBackend` / `IncrementalPirBackend` trait methods
//! directly — every one of them is `pub`, and the upstream crate's own
//! `ikpir-client/tests/response_rewind.rs` already proves the underlying
//! identity holds bit-exactly. Zero upstream changes (`docs/plan.md`
//! ADR-0001, `docs/verification.md` Correction 2). The per-backend index
//! mapping step 2 needs ([`ResponseRewind`]) is copied verbatim from that
//! test file and lives in *this* crate, not upstream.
//!
//! # Hint patching is garbage collection, not synchronisation
//!
//! [`RisePirClient::collect_garbage`] folds the accumulated delta into
//! the hint and advances the pinned block — purely to bound memory and
//! the per-query rewind cost. It is never required for correctness: a
//! hint left unpatched for 500 blocks and a freshly-collected one answer
//! every query identically (`gc_then_query_agrees`).
//!
//! # Scope
//!
//! Library only — no JSON-RPC server yet. [`RisePirClient::finish`]
//! returns [`Lookup`], never a bare number, so callers can distinguish "no
//! matching account" from "decode failed" from "found"; mapping
//! [`Lookup::NotFound`] to `0x0` is a caller policy decision (correct only
//! for a *complete* nonzero-balance database, ADR-0015), deliberately not
//! made here.

mod client;
mod delta;
mod error;
mod rewind;

pub use client::{QueryCtx, RisePirClient};
pub use error::ClientError;
pub use rewind::ResponseRewind;
// `Lookup` is defined in `risepir-proto` (ADR-0009: `ValueCodec::decode`
// returns it directly, and this crate's own scan folds down to the same
// type) — re-exported here so callers of this crate never need to depend
// on `risepir-proto` directly just to name it.
pub use risepir_proto::Lookup;
