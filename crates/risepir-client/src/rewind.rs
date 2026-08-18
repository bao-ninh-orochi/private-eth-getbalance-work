//! [`ResponseRewind`] — the per-backend index mapping for the response
//! rewind's step 2 (`resp -= qᵀ·ΔD`).
//!
//! # Purpose
//!
//! Copied verbatim from the upstream crate's own
//! `ikpir-client/tests/response_rewind.rs` (`rewind_frodo` /
//! `rewind_simple`) — not re-derived. This trait, not upstream, is where
//! the mapping lives: the *policy* this design needs (raw bucket cells
//! before the fingerprint scan, an arbitrary coalesced delta) belongs to
//! this deployment, per `docs/plan.md` §3.2 / ADR-0001.
//!
//! # The mapping
//!
//! - **Frodo** (no reshape): `resp.a[off] -= q.b[row] * d`.
//! - **Simple** (reshape): `big_r = row / k`; `big_c = (row % k) *
//!   row_width + off`; `resp.a[big_c] -= q.b[big_r] * d`, where `k` /
//!   `row_width` are the segment's `SimpleServerParams` reshape
//!   parameters (read off `SimpleClientState::params`, a public field —
//!   no separate parameter needs to be threaded through).
//! - `d as u32` truncating an `i64` to its low 32 bits **is** the correct
//!   reduction mod 2³² — do not "fix" it (matches upstream's own `as u32`
//!   cast on a two's-complement `i64`).

use std::collections::BTreeMap;

use ikpir_common::backend::frodo::{FrodoClientState, FrodoPirBackend, FrodoQuery, FrodoResponse};
use ikpir_common::backend::simple::{
    SimpleClientState, SimplePirBackend, SimpleQuery, SimpleResponse,
};
use ikpir_common::IndexPirBackend;

/// Per-backend response-rewind index mapping (step 2 of `docs/plan.md`
/// §3.3): `resp -= qᵀ·Δ`, where `Δ` is one PIR segment's accumulated
/// `(row, cell_offset) → delta` map.
///
/// Implemented in this crate for both shipped backends — see the module
/// docs for why the mapping lives here and not upstream.
pub trait ResponseRewind: IndexPirBackend {
    /// Subtract `qᵀ·Δ` (mod 2³²) from `resp` in place, for one segment.
    ///
    /// `state` is that segment's [`ClientState`](IndexPirBackend::ClientState) —
    /// threaded through (rather than a bare `ServerParams`) purely so
    /// backends that need reshape parameters (SimplePIR) can read them
    /// off the state's own public `params` field; Frodo's impl ignores
    /// it.
    fn rewind_response(
        state: &Self::ClientState,
        query: &Self::Query,
        resp: &mut Self::Response,
        deltas: &BTreeMap<(u32, u16), i64>,
    );
}

impl ResponseRewind for FrodoPirBackend {
    fn rewind_response(
        _state: &FrodoClientState,
        query: &FrodoQuery,
        resp: &mut FrodoResponse,
        deltas: &BTreeMap<(u32, u16), i64>,
    ) {
        for (&(row, off), &d) in deltas {
            // `as u32` keeps the low 32 bits — exactly reduction mod 2³²
            // for a two's-complement i64, negatives included.
            let term = query.b[row as usize].wrapping_mul(d as u32);
            resp.a[off as usize] = resp.a[off as usize].wrapping_sub(term);
        }
    }
}

impl ResponseRewind for SimplePirBackend {
    fn rewind_response(
        state: &SimpleClientState,
        query: &SimpleQuery,
        resp: &mut SimpleResponse,
        deltas: &BTreeMap<(u32, u16), i64>,
    ) {
        let k = state.params.k;
        let row_width = state.params.row_width;
        for (&(row, off), &d) in deltas {
            let big_r = (row / k) as usize;
            let big_c = ((row % k) * row_width + u32::from(off)) as usize;
            let term = query.b[big_r].wrapping_mul(d as u32);
            resp.a[big_c] = resp.a[big_c].wrapping_sub(term);
        }
    }
}
