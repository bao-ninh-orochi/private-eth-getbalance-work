//! [`RpcError`] — everything that can go wrong inside [`crate::PrivateEth::get_balance`],
//! before it is turned into a JSON-RPC error object by [`crate::rpc`].

use std::fmt;

/// Errors from [`crate::PrivateEth::get_balance`].
///
/// Never converted into a balance — every variant here is a case where
/// `docs/plan.md`'s "never return a wrong answer" invariant means erroring
/// is the only honest option. [`crate::rpc`] maps each variant to a
/// JSON-RPC error object; none of them are ever silently swallowed into
/// `"0x0"`.
#[derive(Debug)]
pub enum RpcError {
    /// The RisePIR HTTP transport failed — network, an unexpected status,
    /// or an undecodable body (see [`risepir_http::ClientError`]).
    Pir(risepir_http::ClientError),
    /// The rewind client rejected the query/response pipeline (see
    /// [`risepir_client::ClientError`]) for a reason other than the
    /// racy-epoch case [`Self::Stalled`] already covers.
    Client(risepir_client::ClientError),
    /// [`risepir_client::Lookup::DecodeFailed`]: a slot's `key_tag`
    /// matched but its balance's checksum did not (`docs/plan.md`
    /// ADR-0009) — LWE decode noise corrupted a value cell. Never
    /// reported as `0x0` or any other number; ADR-0009's whole point is
    /// that this must be a loud error, not a silently wrong balance.
    DecodeFailed,
    /// The queried account is not in this deployment's *partial* tracked
    /// set (a `--partial` bootstrap with no complete snapshot). Absence
    /// only means "zero" for a complete nonzero set (ADR-0015); in a
    /// partial deployment it means "unknown", and `0x0` would be a wrong
    /// answer — so this errors instead, naming the reason.
    NotInTrackedSet,
    /// The client could not catch up to a block it needed (its own last
    /// known head, or the exact block a response claimed to be answered
    /// at) because that range had already aged out of the server's delta
    /// retention window (`GET /sync` returned `409`). Per `docs/plan.md`
    /// §3.6: "client behind / server stalled → label the block; report
    /// stalled; resync outside the ring window; never guess" — so this is
    /// surfaced as an error rather than silently querying against a stale
    /// or mismatched epoch.
    Stalled,
}

impl fmt::Display for RpcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Pir(e) => write!(f, "PIR transport error: {e}"),
            Self::Client(e) => write!(f, "PIR client error: {e}"),
            Self::DecodeFailed => write!(f, "value decode failed (checksum mismatch)"),
            Self::NotInTrackedSet => {
                write!(f, "account is not in this partial deployment's tracked set")
            }
            Self::Stalled => write!(f, "client fell behind the server's delta retention window"),
        }
    }
}

impl std::error::Error for RpcError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Pir(e) => Some(e),
            Self::Client(e) => Some(e),
            Self::DecodeFailed | Self::NotInTrackedSet | Self::Stalled => None,
        }
    }
}

impl From<risepir_http::ClientError> for RpcError {
    fn from(e: risepir_http::ClientError) -> Self {
        Self::Pir(e)
    }
}

impl From<risepir_client::ClientError> for RpcError {
    fn from(e: risepir_client::ClientError) -> Self {
        Self::Client(e)
    }
}
