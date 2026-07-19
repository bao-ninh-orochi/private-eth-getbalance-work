#![forbid(unsafe_code)]
#![warn(missing_docs)]
//! Batched RisePIR server library: one epoch and one delta bundle per
//! Ethereum block (see `docs/plan.md`, esp. §3.2 "Build on the
//! primitives, not the wrappers", §3.4, §3.6; `docs/verification.md`
//! Correction 3). Library only — no HTTP yet.
//!
//! # Scope
//!
//! This crate builds directly on `segmented_cuckoo::CuckooKVStore` and
//! `ikpir_common`'s `IndexPirBackend` / `IncrementalPirBackend` traits —
//! the upstream crate's public *contributions* — rather than on
//! `ikpir_server::IkpirServer`, its thin orchestration wrapper. See
//! [`RisePirServer`]'s module docs for exactly why.
//!
//! # Why not `ikpir_server::IkpirServer`
//!
//! `IkpirServer::{insert,update,delete}` each call a private
//! `commit_mutations()` that drains the SCF mutation log, folds it,
//! patches the hint, and bumps the epoch — **per call**. A 300-change
//! block would therefore cost 300 folds, 300 hint patches, and 300
//! epochs. This design needs exactly **one epoch and one delta bundle
//! per block** (`docs/plan.md` §3.4), and there is no batched entry
//! point upstream — the batch-mutation API is itself the real upstream
//! gap `docs/verification.md` Correction 3 identifies. So this crate:
//!
//! - reimplements the ~50-line, crate-private
//!   `hint_patch::fold_mutations_into_row_deltas` over
//!   `segmented_cuckoo`'s public types (`fold`, private to this crate;
//!   tested against a real `IkpirServer` in the `batched_equals_per_mutation`
//!   test alongside [`RisePirServer`]);
//! - mirrors `IkpirServer`'s per-segment `server_setup` / `server_answer`
//!   / `server_patch_hint` plumbing directly ([`RisePirServer`]);
//! - drops the per-query epoch check entirely ([`RisePirServer::answer`])
//!   — sound because a PIR query depends only on geometry and LWE
//!   dimension, never on the database contents, so the client rewinds
//!   the response against the delta stream instead of needing the
//!   server to be at any particular block (`docs/plan.md` §3.3).
//!
//! Measured 2.1–2.3× faster than 300 individual `IkpirServer` calls
//! (`docs/verification.md` Correction 3) — a deliberate, measured
//! decision, not a shortcut.
//!
//! # The invariant everything here serves
//!
//! Per `docs/plan.md`: *never return a wrong answer*. Concretely:
//! [`RisePirServer::apply_block`] hard-fails on an overflowing balance
//! rather than truncating it, and rejects a whole block (see that
//! method's docs for exactly what "rejects" does and does not guarantee)
//! rather than leaking a partial delta into an unrelated later block;
//! [`DeltaRing::range`] returns `None` — never a delta that silently
//! omits part of the requested span — for any block range it cannot
//! fully reconstruct from its retained window.

mod delta_ring;
mod error;
mod fold;
mod server;
mod verified;

pub use delta_ring::{DeltaRing, DEFAULT_CAPACITY};
pub use error::ServerError;
pub use server::{RisePirServer, SetupBundle};
