//! [`ClientError`] — errors returned by [`crate::RisePirClient`].

use std::fmt;

/// Errors returned by [`crate::RisePirClient`] methods.
///
/// Every variant exists so a caller-side or pipeline problem fails as a
/// clean `Err` rather than ever silently returning a wrong balance — the
/// invariant `docs/plan.md` puts above everything else. [`crate::RisePirClient::finish`]
/// itself distinguishes "found", "not found", and "decode failed" via
/// [`crate::Lookup`]; `ClientError` is reserved for calls that cannot even
/// attempt an honest answer (malformed input, an out-of-sync delta, or an
/// internal consistency check firing).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientError {
    /// [`crate::RisePirClient::ingest_delta`] was given a delta that does
    /// not strictly extend the client's current pending-delta head
    /// forward. Covers a stale or duplicate delta (`d.block <=` the
    /// current head) — the only shape of "does not extend the chain
    /// forward" this type can detect, since a
    /// [`risepir_proto::BlockDelta`] carries no "from" block. An honest
    /// caller may therefore jump the head forward by any amount (a
    /// single block, or a pre-coalesced many-block span fetched via a
    /// future `sync`-style call) but must never go backwards or repeat a
    /// block already folded in.
    NonContiguousDelta {
        /// The pending delta's current head block.
        current_head: u64,
        /// The block number the rejected delta carried.
        found: u64,
    },
    /// A delta's segment count did not match this deployment's arity.
    SegmentCountMismatch {
        /// Expected segment count (the deployment's arity).
        expected: usize,
        /// Segment count the delta actually carried.
        found: usize,
    },
    /// A per-cell running sum in the pending delta overflowed `i64`. Not
    /// reachable with real PIR cell deltas (bounded by the plaintext
    /// modulus, `< 2^31`), but checked rather than assumed — mirrors
    /// [`risepir_proto::CoalesceError::Overflow`].
    DeltaOverflow,
    /// [`crate::RisePirClient::collect_garbage`] was asked to garbage
    /// collect up to a block other than exactly the pending delta's
    /// current head. Partial garbage collection is not supported by this
    /// type's flat rolling-sum representation of `ΔD` — see that
    /// method's docs for why.
    GcTargetMismatch {
        /// The pending delta's current head — the only block
        /// `collect_garbage` can currently target.
        pending_head: u64,
        /// The block number actually requested.
        requested: u64,
    },
    /// [`crate::RisePirClient::finish`] was given a response bundle whose
    /// per-segment length does not match this deployment's arity (or,
    /// defensively, a [`crate::QueryCtx`] whose internal lengths don't).
    MalformedResponse {
        /// Expected segment count (the deployment's arity).
        expected: usize,
        /// Segment count actually supplied.
        found: usize,
    },
    /// [`crate::RisePirClient::finish`] was called with `at_block`
    /// different from the pending delta's current head — the response
    /// was computed at a block the client's own `ΔD` does not (yet, or
    /// any longer) exactly cover, so the rewind cannot be performed
    /// bit-exactly. Never guessed; see `docs/plan.md` §3.6 ("client
    /// behind / server stalled → label the block; report stalled; never
    /// guess").
    ResponseBlockMismatch {
        /// The pending delta's current head.
        pending_head: u64,
        /// The block the response claimed to be answered at.
        response_block: u64,
    },
    /// [`crate::RisePirClient::finish`] was given a `key` whose
    /// candidate-bucket rows don't match the [`crate::QueryCtx`] it was
    /// paired with. Always a caller bug — pairing the `key` and `ctx`
    /// from the same [`crate::RisePirClient::build_query`] call never
    /// triggers this; catches accidentally swapped `(key, ctx)` pairs
    /// before they can apply a delta correction to the wrong row.
    KeyContextMismatch,
    /// After applying the pending delta to a decoded bucket cell (step 4
    /// of the rewind), the corrected value fell outside
    /// `[0, 2^plaintext_bits)`. This is the free integrity check
    /// `docs/verification.md` licenses from the telescoping bound on
    /// coalesced deltas; firing indicates a pipeline bug (a corrupted or
    /// mis-attributed delta, a geometry mismatch), never legitimate
    /// data. This crate fails loudly here rather than ever returning a
    /// number derived from an out-of-range cell.
    CellOutOfRange {
        /// Segment index the offending cell was in.
        segment: usize,
        /// Row (within the segment) the offending cell was in.
        row: u32,
        /// Cell offset within the row.
        offset: u16,
    },
}

impl fmt::Display for ClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NonContiguousDelta { current_head, found } => write!(
                f,
                "non-contiguous delta: pending head is {current_head}, delta claims block {found}"
            ),
            Self::SegmentCountMismatch { expected, found } => {
                write!(f, "segment count mismatch: expected {expected}, found {found}")
            }
            Self::DeltaOverflow => write!(f, "pending delta accumulator overflowed i64"),
            Self::GcTargetMismatch { pending_head, requested } => write!(
                f,
                "collect_garbage target mismatch: pending delta head is {pending_head}, requested {requested}"
            ),
            Self::MalformedResponse { expected, found } => {
                write!(f, "malformed response: expected {expected} segments, found {found}")
            }
            Self::ResponseBlockMismatch { pending_head, response_block } => write!(
                f,
                "response answered at block {response_block} but pending delta covers up to {pending_head}"
            ),
            Self::KeyContextMismatch => {
                write!(f, "key does not match the query context it was paired with")
            }
            Self::CellOutOfRange { segment, row, offset } => write!(
                f,
                "corrected cell out of range at segment {segment} row {row} offset {offset}"
            ),
        }
    }
}

impl std::error::Error for ClientError {}
