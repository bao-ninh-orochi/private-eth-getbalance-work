//! The browser client's state machine — everything the host ABI in
//! [`crate`] is a thin byte-shuffling wrapper around.
//!
//! This is deliberately the *same* sequence `risepir-rpc`'s native front
//! end runs (`risepir_rpc::private_eth::PrivateEth::get_balance`), with
//! one difference: there, the front end owns an async HTTP client and can
//! call `/head`, `/sync` and `/answer` itself; here the host does the I/O
//! and feeds the bytes in. So what is a single `get_balance` there is a
//! four-step conversation here — [`Session::build_query`],
//! [`Session::accept_answer`], [`Session::ingest`], [`Session::finish`] —
//! and each step re-checks the invariants rather than trusting the host to
//! have called them in the right order.
//!
//! # The ordering that must not be got wrong
//!
//! `docs/plan.md` §3.3: the response is rewound against the client's
//! accumulated `ΔD`, and [`RisePirClient::finish`] requires that
//! accumulator's head to equal *exactly* the block the server answered at.
//! A client that skipped the sync would decode against the wrong span and
//! return a wrong balance — silently. [`Session::finish`] therefore
//! refuses to run until `pending_head == at_block`
//! ([`SessionError::SyncRequired`], which names the block the host must
//! sync to) instead of proceeding on a guess.

use ikpir_common::backend::simple::SimpleResponse;
use ikpir_common::SimplePirBackend;
use risepir_client::{ClientError, Lookup, QueryCtx, RisePirClient};
use risepir_http::wire::{self, WireError};
use risepir_proto::{codec::CodecError, keccak256, AddressHash, ValueCodec};
use risepir_server::SetupBundle;

/// Parses a `GET /mode` body (or the equivalent single header byte the
/// host converted): exactly `[0]` (partial) or `[1]` (complete).
///
/// # Errors
///
/// [`SessionError::BadModeByte`] for anything else — the completeness
/// flag decides whether absence means `0x0`, so a malformed one is fatal
/// rather than defaulted (ADR-0015/0017: "never guessed").
pub fn parse_mode(mode_bytes: &[u8]) -> Result<bool, SessionError> {
    match mode_bytes {
        [0] => Ok(false),
        [1] => Ok(true),
        other => Err(SessionError::BadModeByte(other.len())),
    }
}

/// The value encoding this deployment uses (`docs/plan.md` §3.5,
/// ADR-0009). Must match the server's, which `risepir-rpc` builds from the
/// same three widths; they are protocol constants, not tunables a client
/// gets to pick, and a mismatch would decode a valid balance into garbage.
/// Kept here rather than imported so this crate does not depend on the
/// binary crate.
fn value_codec() -> ValueCodec {
    ValueCodec {
        key_tag_bits: 32,
        balance_bits: 96,
        checksum_bits: 16,
    }
}

/// What a completed lookup resolved to. The distinction between [`Zero`]
/// and [`Untracked`] is `docs/plan.md`'s central honesty rule and is
/// decided by the server-declared completeness flag, never by the client.
///
/// [`Zero`]: Outcome::Zero
/// [`Untracked`]: Outcome::Untracked
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The account is in the set, with this balance in wei.
    Found(u128),
    /// Absent from a **complete** nonzero-balance set — which is exactly
    /// `0x0`, since absent ⟺ zero for a complete set (ADR-0015).
    Zero,
    /// Absent from a **partial** set. Absence carries no information here:
    /// the account may hold any balance and simply predate the bootstrap.
    /// Must surface as an error, never as `0x0` (ADR-0015/0017).
    Untracked,
    /// The value's checksum did not verify: the balance cells did not
    /// decode cleanly. An error, never a number (`docs/plan.md` §3.5).
    DecodeFailed,
}

/// Everything that can go wrong, as a legible message for the host.
#[derive(Debug)]
pub enum SessionError {
    /// `GET /mode`'s body was not exactly one byte of `0` or `1`. The
    /// completeness flag decides whether absence means `0x0`, so a
    /// malformed or missing one is fatal rather than defaulted
    /// (ADR-0015/0017: "never guessed").
    BadModeByte(usize),
    /// An address that was not exactly 20 bytes.
    BadAddressLen(usize),
    /// A second `build_query` before the first was finished. The backend
    /// keeps one in-flight slot per segment, so this would discard the
    /// first query's secret and decode the first lookup into garbage.
    QueryAlreadyInFlight,
    /// `accept_answer`/`finish` with no query outstanding.
    NoQueryInFlight,
    /// `finish` before the host synced the client's delta up to the block
    /// the response was answered at. Carries that block.
    SyncRequired {
        /// The block the server answered the query at.
        at_block: u64,
        /// How far the client's accumulated delta currently reaches.
        pending_head: u64,
    },
    /// `finish` before any answer bytes were accepted.
    NoAnswer,
    /// The session was never initialised.
    NotInitialised,
    /// Wire codec rejection (setup, query, or response bundle).
    Wire(WireError),
    /// Delta codec rejection.
    Codec(CodecError),
    /// The rewind client rejected the operation.
    Client(ClientError),
}

impl core::fmt::Display for SessionError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::BadModeByte(n) => write!(
                f,
                "GET /mode returned {n} bytes; expected exactly one byte, 0 (partial) or 1 (complete)"
            ),
            Self::BadAddressLen(n) => write!(f, "address must be exactly 20 bytes, got {n}"),
            Self::QueryAlreadyInFlight => write!(
                f,
                "a query is already in flight; finish it before building another (one in-flight query per segment)"
            ),
            Self::NoQueryInFlight => write!(f, "no query in flight"),
            Self::SyncRequired { at_block, pending_head } => write!(
                f,
                "response was answered at block {at_block} but the client's delta only reaches {pending_head}; \
                 sync (from={pending_head}, to={at_block}) and call finish again"
            ),
            Self::NoAnswer => write!(f, "no answer accepted for the in-flight query"),
            Self::NotInitialised => write!(f, "session not initialised; load GET /mode and GET /setup first"),
            Self::Wire(e) => write!(f, "wire: {e}"),
            Self::Codec(e) => write!(f, "delta codec: {e}"),
            Self::Client(e) => write!(f, "client: {e}"),
        }
    }
}

impl From<WireError> for SessionError {
    fn from(e: WireError) -> Self {
        Self::Wire(e)
    }
}
impl From<CodecError> for SessionError {
    fn from(e: CodecError) -> Self {
        Self::Codec(e)
    }
}
impl From<ClientError> for SessionError {
    fn from(e: ClientError) -> Self {
        Self::Client(e)
    }
}

/// A query that has been built and not yet finished.
struct InFlight {
    key: AddressHash,
    ctx: QueryCtx<SimplePirBackend>,
    /// The decoded `POST /answer` result, once the host has supplied it.
    answer: Option<(Vec<SimpleResponse>, u64)>,
}

/// One browser client, pinned to one deployment's setup bundle.
pub struct Session {
    client: RisePirClient<SimplePirBackend>,
    /// Per-segment `reshape_row_width` — the exact length each response
    /// segment must have. Held because [`wire::decode_response_bundle`]
    /// checks lengths exactly (not as an upper bound), which is what keeps
    /// a malformed `/answer` body from reaching `client_decode`'s
    /// unconditional slicing.
    reshape_row_width_per_seg: Vec<u32>,
    arity: usize,
    plaintext_bits: u32,
    /// Server-declared: does it hold the complete nonzero-balance set?
    complete: bool,
    /// The lineage token of the bundle this session bootstrapped from
    /// ([`wire::lineage_epoch`], ADR-0033) — 16 lowercase-hex chars the
    /// host must attach as `?epoch=` to every `/sync` and `/answer`
    /// request, so the server refuses (409) to feed this session another
    /// bootstrap's deltas or answers (which would decode to garbage
    /// against this hint, and garbage can surface as a silent `0x0` in
    /// complete mode).
    epoch: String,
    /// The block the client's accumulated `ΔD` currently reaches. Tracked
    /// here for the same reason `PrivateEth` tracks its own copy:
    /// `RisePirClient` keeps it as accumulator-internal bookkeeping and
    /// deliberately does not publish it.
    pending_head: u64,
    pinned_block: u64,
    in_flight: Option<InFlight>,
}

impl Session {
    /// Bootstrap from the two bodies a fresh client fetches: `GET /mode`
    /// (one byte) and `GET /setup` (the full bundle).
    ///
    /// Equals [`Self::decode`] followed by [`Self::from_bundle`]; the ABI
    /// host calls those two directly instead, so it can free the encoded
    /// input buffer between them (see `risepir_init`) — this composed
    /// form stays for anyone holding both byte slices at once.
    ///
    /// # Errors
    ///
    /// [`SessionError::BadModeByte`] for a mode body that is not exactly
    /// `[0]` or `[1]`; [`SessionError::Wire`] for any malformed setup
    /// bundle — including a truncated download, which is the realistic
    /// failure for a ~50 MB body over a flaky connection and must be an
    /// error rather than a half-initialised client.
    pub fn new(setup_bytes: &[u8], mode_bytes: &[u8]) -> Result<Self, SessionError> {
        let complete = parse_mode(mode_bytes)?;
        let bundle = Self::decode(setup_bytes)?;
        Ok(Self::from_bundle(bundle, complete))
    }

    /// Step one of [`Self::new`]: decode the `GET /setup` body into its
    /// owned bundle, without building anything. Split out so the host
    /// can release the *encoded* bytes (~831 MB at the complete mainnet
    /// set) before [`Self::from_bundle`] allocates the decoded client —
    /// in wasm, whose linear memory never shrinks, holding both across
    /// the build would permanently cost the tab one extra hint-set of
    /// footprint (ADR-0032's peak estimate is derived from this exact
    /// sequence).
    ///
    /// # Errors
    ///
    /// [`SessionError::Wire`] as on [`Self::new`].
    pub fn decode(setup_bytes: &[u8]) -> Result<SetupBundle<SimplePirBackend>, SessionError> {
        Ok(wire::decode_setup(setup_bytes)?)
    }

    /// Step two of [`Self::new`]: build the session from an
    /// already-decoded bundle plus the (separately validated — see
    /// [`parse_mode`]) completeness flag.
    pub fn from_bundle(bundle: SetupBundle<SimplePirBackend>, complete: bool) -> Self {
        let arity = bundle.params.arity();
        let plaintext_bits = bundle.params.plaintext_bits;
        let reshape_row_width_per_seg: Vec<u32> = bundle
            .backend_params
            .iter()
            .map(|sp| sp.reshape_row_width)
            .collect();
        let epoch = wire::lineage_epoch(&bundle.backend_params);
        let pinned_block = bundle.block;

        let client = RisePirClient::from_setup(bundle, value_codec());

        Self {
            client,
            reshape_row_width_per_seg,
            arity,
            plaintext_bits,
            complete,
            epoch,
            pending_head: pinned_block,
            pinned_block,
            in_flight: None,
        }
    }

    /// Whether the deployment declared a complete nonzero-balance set.
    pub const fn complete(&self) -> bool {
        self.complete
    }
    /// The bundle's lineage token (16 lowercase-hex chars) — see the
    /// field's docs; the host attaches this to `/sync` and `/answer`.
    pub fn epoch(&self) -> &str {
        &self.epoch
    }
    /// The block the hint is pinned at.
    pub const fn pinned_block(&self) -> u64 {
        self.pinned_block
    }
    /// The block the accumulated delta reaches — the `from` of the next
    /// `GET /sync`.
    pub const fn pending_head(&self) -> u64 {
        self.pending_head
    }
    /// Segment count (SCF arity).
    pub const fn arity(&self) -> usize {
        self.arity
    }
    /// Nonzero cells currently pending in `ΔD` — what a garbage collect
    /// would fold away, and a useful "how stale is this client" readout.
    pub fn delta_cells(&self) -> usize {
        self.client.delta_cells()
    }

    /// Fold a `GET /sync` body (a coalesced `(from, to]` delta) into the
    /// client's rolling `ΔD`. Returns the new pending head.
    ///
    /// # Errors
    ///
    /// [`SessionError::Codec`] if the bytes do not decode (every length is
    /// validated before allocation by `risepir_proto::codec`), or
    /// [`SessionError::Client`] if the delta does not extend the
    /// accumulator strictly forward — a stale, duplicated, or
    /// out-of-order delta is rejected rather than applied twice.
    pub fn ingest(&mut self, delta_bytes: &[u8]) -> Result<u64, SessionError> {
        let delta = risepir_proto::codec::decode_block_delta(
            delta_bytes,
            self.plaintext_bits,
            self.arity as u32,
        )?;
        let head = delta.block;
        self.client.ingest_delta(&delta)?;
        self.pending_head = head;
        Ok(head)
    }

    /// Build the per-segment LWE query for a 20-byte address, returning
    /// the `POST /answer` body.
    ///
    /// The address is hashed to the SCF key here (`keccak256`, ADR-0008)
    /// and never leaves this module: what the host gets back is the
    /// encoded query bundle, which is LWE ciphertext.
    ///
    /// # Errors
    ///
    /// [`SessionError::BadAddressLen`], or
    /// [`SessionError::QueryAlreadyInFlight`] if a previous query has not
    /// been finished — see that variant's docs for why this is refused
    /// rather than allowed to overwrite.
    pub fn build_query(&mut self, addr: &[u8]) -> Result<Vec<u8>, SessionError> {
        if self.in_flight.is_some() {
            return Err(SessionError::QueryAlreadyInFlight);
        }
        let addr20: [u8; 20] = addr
            .try_into()
            .map_err(|_| SessionError::BadAddressLen(addr.len()))?;
        let key = keccak256(&addr20);
        let (queries, ctx) = self.client.build_query(&key);
        let body = wire::encode_query_bundle(&queries);
        self.in_flight = Some(InFlight {
            key,
            ctx,
            answer: None,
        });
        Ok(body)
    }

    /// Decode a `POST /answer` response body against this deployment's
    /// exact per-segment geometry. Returns the block the server answered
    /// at, which the host must then sync the delta up to before
    /// [`Self::finish`].
    ///
    /// # Errors
    ///
    /// [`SessionError::NoQueryInFlight`], or [`SessionError::Wire`] for
    /// any malformed or wrong-length body.
    pub fn accept_answer(&mut self, resp_bytes: &[u8]) -> Result<u64, SessionError> {
        let decoded =
            wire::decode_response_bundle(resp_bytes, &self.reshape_row_width_per_seg, self.arity)?;
        let at_block = decoded.1;
        let slot = self
            .in_flight
            .as_mut()
            .ok_or(SessionError::NoQueryInFlight)?;
        slot.answer = Some(decoded);
        Ok(at_block)
    }

    /// Rewind, decode, and scan — steps 2–5 of `docs/plan.md` §3.3.
    ///
    /// Consumes the in-flight query on success *and* on any failure that
    /// leaves it unusable; the one recoverable case
    /// ([`SessionError::SyncRequired`]) leaves everything intact so the
    /// host can sync and call this again.
    ///
    /// # Errors
    ///
    /// [`SessionError::NoQueryInFlight`] / [`SessionError::NoAnswer`] for
    /// out-of-order use, [`SessionError::SyncRequired`] as above, and
    /// [`SessionError::Client`] if the rewind itself rejects the inputs.
    pub fn finish(&mut self) -> Result<Outcome, SessionError> {
        let slot = self
            .in_flight
            .as_ref()
            .ok_or(SessionError::NoQueryInFlight)?;
        let (_, at_block) = slot.answer.as_ref().ok_or(SessionError::NoAnswer)?;

        // Checked *before* taking the slot, so this error is recoverable:
        // the host syncs, then calls finish again with the same answer.
        if *at_block != self.pending_head {
            return Err(SessionError::SyncRequired {
                at_block: *at_block,
                pending_head: self.pending_head,
            });
        }

        let InFlight { key, ctx, answer } = self.in_flight.take().expect("checked above");
        let (responses, at_block) = answer.expect("checked above");
        let lookup = self.client.finish(&key, &ctx, responses, at_block)?;

        Ok(match lookup {
            Lookup::Found(balance) => Outcome::Found(balance),
            // The one place the completeness flag is consulted, and it is
            // the server's, never a default (ADR-0015/0017).
            Lookup::NotFound if self.complete => Outcome::Zero,
            Lookup::NotFound => Outcome::Untracked,
            Lookup::DecodeFailed => Outcome::DecodeFailed,
        })
    }
}
