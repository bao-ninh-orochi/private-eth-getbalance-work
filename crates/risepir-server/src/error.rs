//! [`ServerError`] — errors returned by [`crate::RisePirServer`].

use std::fmt;

use risepir_proto::ValueError;

/// Errors returned by [`crate::RisePirServer`] methods.
///
/// Every variant is designed so that ingest-time or query-time problems
/// come back as a clean `Err` rather than a panic, an OOM, or — worst of
/// all under `docs/plan.md`'s "never return a wrong answer" invariant — a
/// plausible-looking wrong balance.
///
/// The two variants that can leave [`crate::RisePirServer`] mid-block
/// (`BalanceOverflow`/`InvalidWidth` via [`Self::Encode`], and
/// [`Self::TableFull`]) do **not** imply the whole call was a no-op — see
/// [`crate::RisePirServer::apply_block`]'s documentation for exactly what
/// store/hint state results and what recovery looks like. This crate
/// never claims transactional atomicity it does not implement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerError {
    /// A balance failed to encode via [`risepir_proto::ValueCodec::encode`]:
    /// either it does not fit in the configured `balance_bits`
    /// ([`ValueError::BalanceOverflow`] — the balance is never truncated,
    /// `docs/plan.md` ADR-0009), or the codec's own widths are
    /// unrepresentable ([`ValueError::InvalidWidth`] — a deployment
    /// configuration bug, since [`crate::RisePirServer::new`] already
    /// checks the codec's byte width against the store's on
    /// construction; reaching this at `apply_block` time would mean the
    /// codec was mutated after construction, which nothing in this
    /// crate's public API allows).
    Encode(ValueError),
    /// The cuckoo store's kick budget was exhausted while placing a new
    /// key (`key` not present under any of the `arity` candidate
    /// buckets). See [`crate::RisePirServer::apply_block`] and
    /// [`crate::RisePirServer::full_rebuild`] for recovery — the same
    /// recovery `IkpirServer::insert`'s `TableFull` documents: rebuild at
    /// a larger `num_buckets` from authoritative `(address, balance)`
    /// state, since the cuckoo store itself does not retain keys. Note
    /// that in the *deployed* topology this recovery means a process
    /// re-bootstrap, not an in-place `full_rebuild` call — see that
    /// method's "do not wire this into a serving process" warning for
    /// the `NodeState` caches an in-place rebuild would silently
    /// invalidate.
    TableFull,
    /// [`crate::RisePirServer::apply_block`] was called with
    /// `update.block` not strictly greater than the server's current
    /// block ([`crate::RisePirServer::block`]). Checked *before* any
    /// store mutation is attempted, so this variant alone never requires
    /// any recovery — the store and hints are left exactly as they were.
    NonMonotonicBlock {
        /// The server's current block at the time of the call.
        current: u64,
        /// The block number that was rejected.
        attempted: u64,
    },
    /// [`crate::RisePirServer::answer`] was called with the wrong number
    /// of per-segment queries for this deployment's arity. Validated
    /// before any segment is touched.
    MalformedQuery {
        /// Expected query count (the deployment's arity).
        expected: usize,
        /// Actual query count supplied.
        got: usize,
    },
    /// The verified candidate-bucket scan (ADR-0017,
    /// `crate::verified`) found this key's entry sitting *behind* a
    /// foreign fingerprint-colliding slot in probe order
    /// (`shadowed: true`), or found more than one slot matching both the
    /// fingerprint and the `key_tag` (`shadowed: false`). Either way the
    /// store's key-addressed first-match ops would act on the wrong slot,
    /// so the block is rejected loudly instead — never a silent
    /// corruption of a colliding account's entry. Recovery: re-bootstrap
    /// from the authoritative snapshot source (ADR-0011); at 32-bit
    /// fingerprints this is a ~2⁻³²-per-colliding-pair event, expected
    /// years apart even at mainnet change rates.
    FingerprintAmbiguity {
        /// `true`: the key's own entry exists but is preceded by a
        /// foreign fp-match. `false`: two slots matched fp + `key_tag`.
        shadowed: bool,
    },
    /// A verified read (`crate::verified`) found this key's entry (fp and
    /// `key_tag` both match) but its balance failed the value checksum —
    /// the store's own cells are corrupt. Serving must stop and the
    /// operator re-bootstrap; continuing would risk the exact
    /// silently-wrong-balance the checksum exists to catch.
    CorruptStoredValue,
    /// The store rejected a write for a reason other than `TableFull` /
    /// `NotFound`. In practice this means
    /// [`segmented_cuckoo::CuckooError::InvalidParams`] — a value-length
    /// mismatch between `value_codec` and the store's configured
    /// `value_bits` — surfaced defensively; [`crate::RisePirServer::new`]
    /// already asserts the two agree at construction time, so this
    /// should not be reachable in practice. Also used for the (equally
    /// unreachable per the store's own documented contract, but checked
    /// rather than assumed) cases of `update` returning `TableFull` or
    /// `insert` returning `NotFound`.
    Store(String),
}

impl fmt::Display for ServerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Encode(e) => write!(f, "balance encode failed: {e}"),
            Self::TableFull => write!(f, "table is full: cuckoo kicking budget exhausted"),
            Self::NonMonotonicBlock { current, attempted } => write!(
                f,
                "block must be strictly increasing: server is at {current}, got {attempted}"
            ),
            Self::MalformedQuery { expected, got } => {
                write!(f, "malformed query: expected {expected} segments, got {got}")
            }
            Self::FingerprintAmbiguity { shadowed: true } => write!(
                f,
                "fingerprint ambiguity: the key's entry is shadowed by a foreign \
                 fingerprint-colliding slot earlier in probe order; refusing a write that \
                 would corrupt the colliding account (re-bootstrap from the snapshot source)"
            ),
            Self::FingerprintAmbiguity { shadowed: false } => write!(
                f,
                "fingerprint ambiguity: more than one slot matches this key's fingerprint \
                 AND key_tag; refusing to guess which is authoritative (re-bootstrap from \
                 the snapshot source)"
            ),
            Self::CorruptStoredValue => write!(
                f,
                "stored value corrupt: a slot matching this key's fingerprint and key_tag \
                 failed its balance checksum; refusing to serve from a corrupt store \
                 (re-bootstrap from the snapshot source)"
            ),
            Self::Store(msg) => write!(f, "store rejected write: {msg}"),
        }
    }
}

impl std::error::Error for ServerError {}
