//! SimplePIR-concrete binary wire codec for the HTTP transport
//! (`docs/plan.md` §3.4, ADR-0006).
//!
//! # Purpose
//!
//! `risepir_proto::codec` already lays a wire format over the
//! backend-agnostic `BlockDelta`/`Vec<u32>` payloads (`ikpir_common::wire`
//! ships no serialization *by design* — deployments layer their own). This
//! module is the analogous layer for the three bundles this HTTP transport
//! adds on top: the server's full `SetupBundle<SimplePirBackend>`, a
//! per-segment `SimpleQuery` bundle, and a per-segment `SimpleResponse`
//! bundle (plus the epoch it was answered at).
//!
//! # Untrusted-input discipline
//!
//! `POST /answer`'s body is attacker-controlled, multi-hundred-KB-scale
//! input. Every length or count read from the wire in this module is
//! validated against the number of bytes actually remaining in the input
//! *before* it is used to size a `Vec` allocation — mirroring
//! `risepir_proto::codec`'s own discipline (see that module's docs) and,
//! for the dynamic-length `Vec<u32>` payloads, reusing its
//! [`risepir_proto::codec::read_u32s`] primitive directly rather than
//! re-deriving the same bound-checking logic here.
//!
//! Malformed, truncated, or adversarial input always produces a clean
//! [`WireError`], never a panic and never an attempted large allocation.
//!
//! # One step further than an upper bound: exact per-segment lengths
//!
//! [`decode_query_bundle`] and [`decode_response_bundle`] don't just cap
//! each segment's `Vec<u32>` length at *some* ceiling — they require it to
//! match the deployment's geometry **exactly**. This is not a style choice:
//! `ikpir_common::backend::simple::SimplePirBackend::server_answer`
//! (reached from `RisePirServer::answer`, which this crate's `/answer`
//! handler feeds a decoded query bundle straight into) slices
//! `query.b[..full]` and indexes `query.b[full]` unconditionally — the only
//! guard is a `debug_assert_eq!(query.b.len(), params.reshape_rows)`,
//! compiled out in release builds. A query segment shorter than
//! `reshape_rows` would therefore make the *server* panic on an
//! out-of-bounds index, not return a clean error — exactly the failure
//! mode this whole module exists to prevent. The same reasoning applies to
//! a response segment's length against `reshape_row_width` (consumed by
//! `client_decode`) and a setup bundle's per-segment hint length against
//! `lwe_dim * reshape_row_width` (consumed by the matvec kernel that reads
//! a hint). So every one of those three checks compares with `==`, not
//! `<=`, after bounding the read with `read_u32s`'s own upper bound.

use ikpir_common::backend::simple::{SimpleHint, SimpleParams, SimpleQuery, SimpleResponse, SimpleServerParams};
use ikpir_common::SimplePirBackend;
use risepir_proto::codec::{self, CodecError};
use risepir_server::SetupBundle;
use segmented_cuckoo::{CuckooParams, SchemeKind};

const SETUP_MAGIC: [u8; 4] = *b"RPS1";
const QUERY_MAGIC: [u8; 4] = *b"RPQ1";
const RESPONSE_MAGIC: [u8; 4] = *b"RPR1";

/// Errors from every `decode_*` function in this module.
///
/// Every variant exists so a malformed, truncated, or adversarial byte
/// stream produces a clean `Err` — never a panic, an OOM, or (worst of
/// all) a value that decodes "successfully" into something a downstream
/// PIR call cannot safely index (see the module docs' "exact per-segment
/// lengths" section).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WireError {
    /// Input was shorter than the 4-byte magic, or the magic bytes present
    /// did not match the expected message type.
    BadMagic,
    /// Input ended before a fixed-size field could be fully read.
    UnexpectedEof,
    /// The scheme byte was not `2`, `3`, or `4` (the only arities
    /// [`segmented_cuckoo::SchemeKind`] has).
    BadScheme(u8),
    /// A declared segment count did not match the expected arity.
    ArityMismatch {
        /// Arity the caller expected.
        expected: usize,
        /// Segment count the message actually declared.
        found: usize,
    },
    /// A segment's `Vec<u32>` payload (a query, response, or hint) decoded
    /// to a length shorter than this deployment's geometry requires. See
    /// the module docs' "exact per-segment lengths" section for why this
    /// is checked at all, rather than merely bounding from above.
    SegmentLengthMismatch {
        /// Which segment (0-indexed).
        segment: usize,
        /// Exact length required.
        expected: u64,
        /// Length actually decoded.
        found: usize,
    },
    /// A setup bundle segment's `lwe_dim` / `plaintext_bits` / `sigma`
    /// would make [`ikpir_common::backend::simple::SimpleParams::new`]
    /// panic (it asserts `lwe_dim > 0`, `plaintext_bits` in `1..=31`, and
    /// `sigma` finite and positive). Checked here first so a decoder fed
    /// attacker-controlled bytes always returns `Err` instead of hitting
    /// that assertion.
    InvalidSimpleParams {
        /// Which segment (0-indexed).
        segment: usize,
    },
    /// Bytes remained in the input after a complete, well-formed message
    /// was parsed. Treated as corruption rather than silently ignored.
    TrailingBytes,
    /// A wrapped error from [`risepir_proto::codec`], surfaced while
    /// decoding one of this module's embedded `Vec<u32>` payloads (a
    /// query, response, or hint).
    Codec(CodecError),
}

impl std::fmt::Display for WireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadMagic => write!(f, "bad magic: not a recognised RisePIR HTTP wire message"),
            Self::UnexpectedEof => write!(f, "unexpected end of input"),
            Self::BadScheme(b) => write!(f, "bad scheme byte: {b} (expected 2, 3, or 4)"),
            Self::ArityMismatch { expected, found } => {
                write!(f, "arity mismatch: expected {expected} segments, found {found}")
            }
            Self::SegmentLengthMismatch { segment, expected, found } => write!(
                f,
                "segment {segment}: expected exactly {expected} u32 cells, found {found}"
            ),
            Self::InvalidSimpleParams { segment } => write!(
                f,
                "segment {segment}: lwe_dim/plaintext_bits/sigma out of range for SimplePIR"
            ),
            Self::TrailingBytes => write!(f, "trailing bytes after a complete message"),
            Self::Codec(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for WireError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Codec(e) => Some(e),
            _ => None,
        }
    }
}

impl From<CodecError> for WireError {
    fn from(e: CodecError) -> Self {
        Self::Codec(e)
    }
}

// ─── fixed-size field readers ────────────────────────────────────────────
//
// Every read here is bounds-checked via `slice::get`, which returns `None`
// (turned into `Err`) rather than panicking on an out-of-range index — the
// same "check before touch" discipline `risepir_proto::codec` uses. Because
// each reads a small, fixed number of bytes, none of these can themselves
// be steered into a large allocation; the only dynamically-sized reads in
// this module go through `risepir_proto::codec::read_u32s`, which carries
// its own remaining-bytes bound.

fn expect_magic(bytes: &[u8], magic: &[u8; 4]) -> Result<usize, WireError> {
    if bytes.len() < 4 || bytes[0..4] != *magic {
        return Err(WireError::BadMagic);
    }
    Ok(4)
}

fn read_u8(bytes: &[u8], pos: &mut usize) -> Result<u8, WireError> {
    let b = *bytes.get(*pos).ok_or(WireError::UnexpectedEof)?;
    *pos += 1;
    Ok(b)
}

fn read_u32(bytes: &[u8], pos: &mut usize) -> Result<u32, WireError> {
    let end = pos.checked_add(4).ok_or(WireError::UnexpectedEof)?;
    let chunk: [u8; 4] = bytes.get(*pos..end).ok_or(WireError::UnexpectedEof)?.try_into().expect("slice of length 4");
    *pos = end;
    Ok(u32::from_le_bytes(chunk))
}

fn read_u64(bytes: &[u8], pos: &mut usize) -> Result<u64, WireError> {
    let end = pos.checked_add(8).ok_or(WireError::UnexpectedEof)?;
    let chunk: [u8; 8] = bytes.get(*pos..end).ok_or(WireError::UnexpectedEof)?.try_into().expect("slice of length 8");
    *pos = end;
    Ok(u64::from_le_bytes(chunk))
}

fn read_f64(bytes: &[u8], pos: &mut usize) -> Result<f64, WireError> {
    let end = pos.checked_add(8).ok_or(WireError::UnexpectedEof)?;
    let chunk: [u8; 8] = bytes.get(*pos..end).ok_or(WireError::UnexpectedEof)?.try_into().expect("slice of length 8");
    *pos = end;
    Ok(f64::from_le_bytes(chunk))
}

fn read_seed(bytes: &[u8], pos: &mut usize) -> Result<[u8; 16], WireError> {
    let end = pos.checked_add(16).ok_or(WireError::UnexpectedEof)?;
    let chunk: [u8; 16] = bytes.get(*pos..end).ok_or(WireError::UnexpectedEof)?.try_into().expect("slice of length 16");
    *pos = end;
    Ok(chunk)
}

/// `u32 * u32` widened to `u64` (always exact — the product of two `u32`s
/// never overflows `u64`) and then saturated down to `usize`, for use as a
/// [`codec::read_u32s`] ceiling. Saturating rather than erroring is sound
/// here: `read_u32s` *also* bounds every read by the bytes actually
/// remaining in the input, so a saturated (i.e. effectively unbounded)
/// `max_len` never itself allows an oversized allocation — it only widens
/// the ceiling to "however many bytes are actually present", which is
/// already the correct bound.
fn mul_u32_saturating_usize(a: u32, b: u32) -> usize {
    usize::try_from(u64::from(a) * u64::from(b)).unwrap_or(usize::MAX)
}

// ─── SetupBundle ─────────────────────────────────────────────────────────

/// Encodes a full [`SetupBundle<SimplePirBackend>`] — everything a fresh
/// client needs from [`GET /setup`](crate::node): the SCF geometry, the
/// current block, and per segment a [`SimpleServerParams`] plus its
/// [`SimpleHint`]. The public matrix `A` is *not* included — a client
/// re-expands it from `SimpleParams::seed` in `client_setup`
/// (`ikpir_common::backend::simple`'s own module docs), so there is
/// nothing to carry beyond the seed already inside `SimpleServerParams`.
///
/// # Layout
///
/// `magic("RPS1") ‖ scheme:u8(=arity) ‖ num_buckets:u32 ‖ bucket_size:u32 ‖
/// fingerprint_bits:u32 ‖ value_bits:u32 ‖ plaintext_bits:u32 ‖ block:u64 ‖`
/// then, per segment (`arity` of them): `lwe_dim:u32 ‖ plaintext_bits:u32 ‖
/// sigma:f64(8 LE bytes) ‖ seed:[u8;16] ‖ n_rows:u32 ‖ row_width:u32 ‖ k:u32
/// ‖ reshape_rows:u32 ‖ reshape_row_width:u32`, then, per segment again: the
/// hint as a [`codec::encode_u32s`]-framed `Vec<u32>`.
pub fn encode_setup(bundle: &SetupBundle<SimplePirBackend>) -> Vec<u8> {
    let arity = bundle.params.arity();
    debug_assert_eq!(bundle.backend_params.len(), arity, "encode_setup: backend_params.len() != params.arity()");
    debug_assert_eq!(bundle.hints.len(), arity, "encode_setup: hints.len() != params.arity()");
    assert!(
        arity <= u8::MAX as usize,
        "encode_setup: arity {arity} does not fit the u8 scheme byte"
    );

    // Pre-size from the bundle's own geometry (see `setup_encoded_len`'s
    // docs) instead of growing `out` by repeated `extend_from_slice`: at
    // the deployed complete-mainnet scale a single segment's hint is
    // ~277 MB, and `Vec`'s doubling growth would otherwise copy hundreds
    // of MB per reallocation while transiently holding both the old and
    // new buffers alive at once (ADR-0028, motivated by the `/setup`
    // response cache this crate now keeps — this sizing fix pays off on
    // every cache regeneration, not just once).
    let capacity = setup_encoded_len(bundle);
    let mut out = Vec::with_capacity(capacity);
    out.extend_from_slice(&SETUP_MAGIC);
    out.push(arity as u8);
    out.extend_from_slice(&bundle.params.num_buckets.to_le_bytes());
    out.extend_from_slice(&bundle.params.bucket_size.to_le_bytes());
    out.extend_from_slice(&bundle.params.fingerprint_bits.to_le_bytes());
    out.extend_from_slice(&bundle.params.value_bits.to_le_bytes());
    out.extend_from_slice(&bundle.params.plaintext_bits.to_le_bytes());
    out.extend_from_slice(&bundle.block.to_le_bytes());

    for sp in &bundle.backend_params {
        out.extend_from_slice(&sp.params.lwe_dim.to_le_bytes());
        out.extend_from_slice(&sp.params.plaintext_bits.to_le_bytes());
        out.extend_from_slice(&sp.params.sigma.to_le_bytes());
        out.extend_from_slice(&sp.params.seed);
        out.extend_from_slice(&sp.n_rows.to_le_bytes());
        out.extend_from_slice(&sp.row_width.to_le_bytes());
        out.extend_from_slice(&sp.k.to_le_bytes());
        out.extend_from_slice(&sp.reshape_rows.to_le_bytes());
        out.extend_from_slice(&sp.reshape_row_width.to_le_bytes());
    }
    for hint in &bundle.hints {
        out.extend_from_slice(&codec::encode_u32s(&hint.data));
    }
    debug_assert_eq!(
        out.len(),
        capacity,
        "encode_setup: final length drifted from setup_encoded_len's up-front computation — \
         the two walk the same fields and must stay in step"
    );
    out
}

/// Exact byte length [`encode_setup`] will produce for `bundle`, derived
/// field by field from the bundle itself and never from a hardcoded
/// constant. This is what lets [`encode_setup`] `Vec::with_capacity` the
/// exact final size instead of paying doubling-growth reallocation (see
/// that function's docs).
///
/// The per-segment hint contribution is measured from `hint.data.len()`
/// rather than derived from `lwe_dim * reshape_row_width`. Those two agree
/// for every bundle a server actually produces — it is the same invariant
/// [`decode_setup`] enforces on the way in — but deriving the *encoder's*
/// buffer size from the geometry would quietly make `encode_setup` a
/// validity check on its input, and it is not one: this crate's own
/// decoder tests legitimately encode hand-built bundles whose hints do not
/// match their geometry, precisely so the decoder can be seen to reject
/// them. Measuring what will actually be written keeps the reservation
/// exact for every input, and leaves rejecting a malformed bundle where it
/// belongs — in [`decode_setup`].
fn setup_encoded_len(bundle: &SetupBundle<SimplePirBackend>) -> usize {
    // magic(4) + scheme:u8(1) + num_buckets/bucket_size/fingerprint_bits/
    // value_bits/plaintext_bits: 5 × u32 (20) + block:u64(8) = 33.
    let mut total: u64 = 33;
    // Per segment: lwe_dim:u32(4) + plaintext_bits:u32(4) + sigma:f64(8) +
    // seed:[u8;16](16) + n_rows:u32(4) + row_width:u32(4) + k:u32(4) +
    // reshape_rows:u32(4) + reshape_row_width:u32(4) = 52, all fixed-width.
    total += 52 * bundle.backend_params.len() as u64;
    for hint in &bundle.hints {
        let hint_len = hint.data.len() as u64;
        total += uvarint_len(hint_len) as u64 + hint_len * 4;
    }
    usize::try_from(total).expect("setup_encoded_len: encoded length overflows usize")
}

/// Byte length of [`codec::encode_u32s`]'s LEB128 element-count prefix for
/// `value` elements. Mirrors `risepir_proto::codec`'s own private
/// `uvarint_len` — not exported from that crate, and this module already
/// keeps its own fixed-size field readers (`read_u32`/`read_u64`/… above)
/// separate from that module's internals for the same reason, so
/// reimplementing this five-line function locally keeps the change
/// contained to this crate instead of widening a sibling crate's public
/// surface for one caller.
fn uvarint_len(mut value: u64) -> usize {
    let mut n = 1;
    while value >= 0x80 {
        value >>= 7;
        n += 1;
    }
    n
}

/// Inverse of [`encode_setup`].
///
/// # Errors
///
/// [`WireError::BadMagic`] / [`WireError::UnexpectedEof`] /
/// [`WireError::BadScheme`] / [`WireError::TrailingBytes`] as documented on
/// [`WireError`]. [`WireError::InvalidSimpleParams`] if a segment's
/// `lwe_dim` / `plaintext_bits` / `sigma` would panic inside
/// [`SimpleParams::new`] (checked *before* that constructor is called — see
/// the module docs). [`WireError::SegmentLengthMismatch`] if a segment's
/// decoded hint length is not exactly `lwe_dim * reshape_row_width` (see
/// the module docs' "exact per-segment lengths" section).
///
/// Never panics, regardless of input content: every count is validated
/// against the remaining input length (and, for the `Vec<u32>` payloads,
/// against the deployment geometry) before it is used to size an
/// allocation.
pub fn decode_setup(bytes: &[u8]) -> Result<SetupBundle<SimplePirBackend>, WireError> {
    let mut pos = expect_magic(bytes, &SETUP_MAGIC)?;

    let scheme_byte = read_u8(bytes, &mut pos)?;
    let (scheme_kind, arity) = match scheme_byte {
        2 => (SchemeKind::Segmented2ary, 2usize),
        3 => (SchemeKind::Segmented3ary, 3usize),
        4 => (SchemeKind::Segmented4ary, 4usize),
        other => return Err(WireError::BadScheme(other)),
    };

    let num_buckets = read_u32(bytes, &mut pos)?;
    let bucket_size = read_u32(bytes, &mut pos)?;
    let fingerprint_bits = read_u32(bytes, &mut pos)?;
    let value_bits = read_u32(bytes, &mut pos)?;
    let plaintext_bits = read_u32(bytes, &mut pos)?;
    let params = CuckooParams {
        scheme_kind,
        num_buckets,
        bucket_size,
        fingerprint_bits,
        value_bits,
        plaintext_bits,
    };

    let block = read_u64(bytes, &mut pos)?;

    let mut backend_params: Vec<SimpleServerParams> = Vec::with_capacity(arity);
    for segment in 0..arity {
        let lwe_dim = read_u32(bytes, &mut pos)?;
        let seg_plaintext_bits = read_u32(bytes, &mut pos)?;
        let sigma = read_f64(bytes, &mut pos)?;
        let seed = read_seed(bytes, &mut pos)?;
        let n_rows = read_u32(bytes, &mut pos)?;
        let row_width = read_u32(bytes, &mut pos)?;
        let k = read_u32(bytes, &mut pos)?;
        let reshape_rows = read_u32(bytes, &mut pos)?;
        let reshape_row_width = read_u32(bytes, &mut pos)?;

        // `SimpleParams::new` (below) `assert!`s exactly these three
        // preconditions and panics if they fail — unacceptable for a
        // decoder fed attacker-controlled bytes, so they are checked here
        // first and turned into a clean `Err` (see `WireError::InvalidSimpleParams`).
        if lwe_dim == 0 || !(1..=31).contains(&seg_plaintext_bits) || !(sigma > 0.0 && sigma.is_finite()) {
            return Err(WireError::InvalidSimpleParams { segment });
        }

        backend_params.push(SimpleServerParams {
            params: SimpleParams::new(lwe_dim, seg_plaintext_bits, sigma, seed),
            n_rows,
            row_width,
            k,
            reshape_rows,
            reshape_row_width,
        });
    }

    let mut hints: Vec<SimpleHint> = Vec::with_capacity(arity);
    for (segment, sp) in backend_params.iter().enumerate() {
        // `SimpleHint::data` is `lwe_dim x reshape_row_width` row-major
        // (that type's own docs) — an *exact* length, not merely an upper
        // bound: a mis-sized hint later drives an out-of-bounds slice
        // inside the shared matvec kernel, whose only guard is a
        // `debug_assert!` compiled out in release (see the module docs).
        let expected_len: u64 = u64::from(sp.params.lwe_dim) * u64::from(sp.reshape_row_width);
        let max_len = mul_u32_saturating_usize(sp.params.lwe_dim, sp.reshape_row_width);
        let data = codec::read_u32s(bytes, &mut pos, max_len)?;
        if data.len() as u64 != expected_len {
            return Err(WireError::SegmentLengthMismatch {
                segment,
                expected: expected_len,
                found: data.len(),
            });
        }
        hints.push(SimpleHint { data });
    }

    if pos != bytes.len() {
        return Err(WireError::TrailingBytes);
    }

    Ok(SetupBundle {
        params,
        backend_params,
        hints,
        block,
    })
}

// ─── query bundle ────────────────────────────────────────────────────────

/// Encodes a per-segment [`SimpleQuery`] bundle — the `POST /answer` body.
///
/// # Layout
///
/// `magic("RPQ1") ‖ arity:u8` then, per segment: `b` as a
/// [`codec::encode_u32s`]-framed `Vec<u32>`.
///
/// # Panics
///
/// If `queries.len() > 255` (the wire arity byte is `u8`-width; real
/// deployments use arity 2, 3, or 4, so this indicates a caller bug, not
/// adversarial input — this function runs on locally-constructed data,
/// never directly on untrusted bytes, mirroring
/// `risepir_proto::codec::encode_block_delta`'s identical precedent).
pub fn encode_query_bundle(queries: &[SimpleQuery]) -> Vec<u8> {
    assert!(
        queries.len() <= u8::MAX as usize,
        "encode_query_bundle: {} segments does not fit the u8 arity byte",
        queries.len()
    );
    let mut out = Vec::new();
    out.extend_from_slice(&QUERY_MAGIC);
    out.push(queries.len() as u8);
    for q in queries {
        out.extend_from_slice(&codec::encode_u32s(&q.b));
    }
    out
}

/// Inverse of [`encode_query_bundle`] — what the server decodes from
/// `POST /answer`'s body.
///
/// `params.arity()` is the expected segment count. `expected_len_per_seg`
/// is the *exact* required length of each segment's `b` vector — i.e. that
/// segment's `reshape_rows` (`SimpleServerParams::reshape_rows`), one per
/// segment, in segment order. `CuckooParams` alone does not carry the
/// SimplePIR reshape geometry (only `SimpleServerParams` does — the
/// reshape is a backend-internal concept, not an SCF one), so the caller —
/// which holds the `SetupBundle`/`SimpleServerParams` this deployment was
/// built from — passes it in explicitly.
///
/// # Errors
///
/// [`WireError::BadMagic`] / [`WireError::UnexpectedEof`] /
/// [`WireError::TrailingBytes`] as documented on [`WireError`].
/// [`WireError::ArityMismatch`] if the wire's declared segment count does
/// not match `params.arity()`, or if `expected_len_per_seg.len()` does not
/// (a caller-contract violation, not adversarial input, but still returned
/// as `Err` rather than risking an out-of-range index below).
/// [`WireError::SegmentLengthMismatch`] if a segment's decoded `b.len()` is
/// not exactly `expected_len_per_seg[segment]` — see the module docs'
/// "exact per-segment lengths" section for why this must be exact, not
/// merely an upper bound.
///
/// Never panics, regardless of input content.
pub fn decode_query_bundle(
    bytes: &[u8],
    params: &CuckooParams,
    expected_len_per_seg: &[u32],
) -> Result<Vec<SimpleQuery>, WireError> {
    let mut pos = expect_magic(bytes, &QUERY_MAGIC)?;

    let arity_byte = read_u8(bytes, &mut pos)?;
    let arity = arity_byte as usize;
    let expected_arity = params.arity();
    if arity != expected_arity {
        return Err(WireError::ArityMismatch { expected: expected_arity, found: arity });
    }
    if expected_len_per_seg.len() != expected_arity {
        return Err(WireError::ArityMismatch {
            expected: expected_arity,
            found: expected_len_per_seg.len(),
        });
    }

    let mut queries = Vec::with_capacity(arity);
    for (segment, &expected_len) in expected_len_per_seg.iter().enumerate() {
        let max_len = expected_len as usize;
        let b = codec::read_u32s(bytes, &mut pos, max_len)?;
        if b.len() != max_len {
            return Err(WireError::SegmentLengthMismatch {
                segment,
                expected: u64::from(expected_len),
                found: b.len(),
            });
        }
        queries.push(SimpleQuery { b });
    }

    if pos != bytes.len() {
        return Err(WireError::TrailingBytes);
    }
    Ok(queries)
}

// ─── response bundle ─────────────────────────────────────────────────────

/// Encodes a per-segment [`SimpleResponse`] bundle plus the block the
/// server answered at — `docs/plan.md` §3.4: "the response names the
/// epoch". This is the `POST /answer` HTTP response body.
///
/// # Layout
///
/// `magic("RPR1") ‖ arity:u8 ‖ head:u64` then, per segment: `a` as a
/// [`codec::encode_u32s`]-framed `Vec<u32>`.
///
/// # Panics
///
/// If `responses.len() > 255` — see [`encode_query_bundle`]'s identical
/// precedent; this function runs on locally-constructed data, never
/// directly on untrusted bytes.
pub fn encode_response_bundle(responses: &[SimpleResponse], head: u64) -> Vec<u8> {
    assert!(
        responses.len() <= u8::MAX as usize,
        "encode_response_bundle: {} segments does not fit the u8 arity byte",
        responses.len()
    );
    let mut out = Vec::new();
    out.extend_from_slice(&RESPONSE_MAGIC);
    out.push(responses.len() as u8);
    out.extend_from_slice(&head.to_le_bytes());
    for r in responses {
        out.extend_from_slice(&codec::encode_u32s(&r.a));
    }
    out
}

/// Inverse of [`encode_response_bundle`] — what a client decodes from
/// `POST /answer`'s response.
///
/// `expected_len_per_seg` is the *exact* required length of each segment's
/// `a` vector — i.e. that segment's `reshape_row_width`
/// (`SimpleServerParams::reshape_row_width`), one per segment, in segment
/// order (the same shape of geometry `decode_query_bundle` needs, and for
/// the same reason: not carried by `CuckooParams` alone). `arity` is the
/// expected segment count.
///
/// # Errors
///
/// [`WireError::BadMagic`] / [`WireError::UnexpectedEof`] /
/// [`WireError::TrailingBytes`] as documented on [`WireError`].
/// [`WireError::ArityMismatch`] if the wire's declared segment count does
/// not match `arity`, or if `expected_len_per_seg.len()` does not.
/// [`WireError::SegmentLengthMismatch`] if a segment's decoded `a.len()` is
/// not exactly `expected_len_per_seg[segment]` — see the module docs'
/// "exact per-segment lengths" section: `client_decode` slices its
/// argument unconditionally on this assumption.
///
/// Never panics, regardless of input content.
pub fn decode_response_bundle(
    bytes: &[u8],
    expected_len_per_seg: &[u32],
    arity: usize,
) -> Result<(Vec<SimpleResponse>, u64), WireError> {
    let mut pos = expect_magic(bytes, &RESPONSE_MAGIC)?;

    let arity_byte = read_u8(bytes, &mut pos)?;
    let found_arity = arity_byte as usize;
    if found_arity != arity {
        return Err(WireError::ArityMismatch { expected: arity, found: found_arity });
    }
    if expected_len_per_seg.len() != arity {
        return Err(WireError::ArityMismatch { expected: arity, found: expected_len_per_seg.len() });
    }

    let head = read_u64(bytes, &mut pos)?;

    let mut responses = Vec::with_capacity(arity);
    for (segment, &expected_len) in expected_len_per_seg.iter().enumerate() {
        let max_len = expected_len as usize;
        let a = codec::read_u32s(bytes, &mut pos, max_len)?;
        if a.len() != max_len {
            return Err(WireError::SegmentLengthMismatch {
                segment,
                expected: u64::from(expected_len),
                found: a.len(),
            });
        }
        responses.push(SimpleResponse { a });
    }

    if pos != bytes.len() {
        return Err(WireError::TrailingBytes);
    }
    Ok((responses, head))
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── dependency-free deterministic PRNG, mirroring the pattern already
    // established in `risepir_proto::codec`'s and `risepir-server`'s own
    // test modules ──────────────────────────────────────────────────────

    fn xorshift(state: &mut u64) -> u64 {
        *state ^= *state << 13;
        *state ^= *state >> 7;
        *state ^= *state << 17;
        *state
    }

    fn random_bytes(state: &mut u64, len: usize) -> Vec<u8> {
        (0..len).map(|_| (xorshift(state) % 256) as u8).collect()
    }

    // ── fixtures ─────────────────────────────────────────────────────────

    fn sample_cuckoo_params() -> CuckooParams {
        CuckooParams {
            scheme_kind: SchemeKind::Segmented3ary,
            num_buckets: 96,
            bucket_size: 4,
            fingerprint_bits: 32,
            value_bits: 96,
            plaintext_bits: 8,
        }
    }

    fn sample_setup_bundle() -> SetupBundle<SimplePirBackend> {
        let arity = 3usize;
        let lwe_dim = 4u32;
        let reshape_row_width = 6u32;
        let backend_params: Vec<SimpleServerParams> = (0..arity)
            .map(|i| SimpleServerParams {
                params: SimpleParams::new(lwe_dim, 8, 6.4, [i as u8; 16]),
                n_rows: 10,
                row_width: 3,
                k: 2,
                reshape_rows: 5,
                reshape_row_width,
            })
            .collect();
        let hints: Vec<SimpleHint> = (0..arity)
            .map(|i| SimpleHint {
                data: (0..lwe_dim * reshape_row_width).map(|j| (i as u32) * 1_000 + j).collect(),
            })
            .collect();
        SetupBundle {
            params: sample_cuckoo_params(),
            backend_params,
            hints,
            block: 42,
        }
    }

    fn sample_queries() -> Vec<SimpleQuery> {
        vec![
            SimpleQuery { b: vec![1, 2, 3, 4, 5] },
            SimpleQuery { b: vec![6, 7, 8, 9, 10] },
            SimpleQuery { b: vec![11, 12, 13, 14, 15] },
        ]
    }

    fn sample_responses() -> Vec<SimpleResponse> {
        vec![
            SimpleResponse { a: vec![100, 200, 300] },
            SimpleResponse { a: vec![400, 500, 600] },
            SimpleResponse { a: vec![700, 800, 900] },
        ]
    }

    // ── round trips ──────────────────────────────────────────────────────

    #[test]
    fn setup_round_trip() {
        let bundle = sample_setup_bundle();
        let bytes = encode_setup(&bundle);
        let decoded = decode_setup(&bytes).unwrap();

        assert_eq!(decoded.params, bundle.params);
        assert_eq!(decoded.block, bundle.block);
        assert_eq!(decoded.backend_params.len(), bundle.backend_params.len());
        for (d, o) in decoded.backend_params.iter().zip(&bundle.backend_params) {
            assert_eq!(d.params.lwe_dim, o.params.lwe_dim);
            assert_eq!(d.params.plaintext_bits, o.params.plaintext_bits);
            assert_eq!(d.params.sigma, o.params.sigma);
            assert_eq!(d.params.seed, o.params.seed);
            assert_eq!(d.n_rows, o.n_rows);
            assert_eq!(d.row_width, o.row_width);
            assert_eq!(d.k, o.k);
            assert_eq!(d.reshape_rows, o.reshape_rows);
            assert_eq!(d.reshape_row_width, o.reshape_row_width);
        }
        assert_eq!(decoded.hints, bundle.hints);
    }

    #[test]
    fn query_bundle_round_trip() {
        let queries = sample_queries();
        let params = sample_cuckoo_params();
        let expected_len_per_seg: Vec<u32> = queries.iter().map(|q| q.b.len() as u32).collect();

        let bytes = encode_query_bundle(&queries);
        let decoded = decode_query_bundle(&bytes, &params, &expected_len_per_seg).unwrap();

        assert_eq!(decoded.len(), queries.len());
        for (d, o) in decoded.iter().zip(&queries) {
            assert_eq!(d.b, o.b);
        }
    }

    #[test]
    fn response_bundle_round_trip() {
        let responses = sample_responses();
        let expected_len_per_seg: Vec<u32> = responses.iter().map(|r| r.a.len() as u32).collect();
        let head = 12_345u64;

        let bytes = encode_response_bundle(&responses, head);
        let (decoded, decoded_head) = decode_response_bundle(&bytes, &expected_len_per_seg, responses.len()).unwrap();

        assert_eq!(decoded_head, head);
        assert_eq!(decoded.len(), responses.len());
        for (d, o) in decoded.iter().zip(&responses) {
            assert_eq!(d.a, o.a);
        }
    }

    // ── targeted rejections ────────────────────────────────────────────

    // `SetupBundle<SimplePirBackend>` is an upstream type this crate must
    // not modify, and it derives neither `Debug` nor `PartialEq` (its own
    // `B` type parameter has no reason to require either) — so
    // `decode_setup`'s negative tests go through this helper, which
    // discards the `Ok` payload via `.map(|_| ())` before calling
    // `.unwrap_err()`, so only `WireError` (which *does* derive both) ever
    // needs to satisfy `Result::unwrap_err`'s `T: Debug` bound.
    fn expect_setup_err(bytes: &[u8]) -> WireError {
        decode_setup(bytes).map(|_| ()).unwrap_err()
    }

    #[test]
    fn decode_setup_rejects_bad_magic() {
        let mut bytes = encode_setup(&sample_setup_bundle());
        bytes[0] ^= 0xff;
        assert_eq!(expect_setup_err(&bytes), WireError::BadMagic);
    }

    #[test]
    fn decode_setup_rejects_short_input() {
        assert_eq!(expect_setup_err(&[]), WireError::BadMagic);
        assert_eq!(expect_setup_err(b"RPS1"), WireError::UnexpectedEof);
    }

    #[test]
    fn decode_setup_rejects_bad_scheme_byte() {
        let mut bytes = encode_setup(&sample_setup_bundle());
        bytes[4] = 9; // not 2, 3, or 4
        assert_eq!(expect_setup_err(&bytes), WireError::BadScheme(9));
    }

    #[test]
    fn decode_setup_rejects_trailing_bytes() {
        let mut bytes = encode_setup(&sample_setup_bundle());
        bytes.push(0);
        assert_eq!(expect_setup_err(&bytes), WireError::TrailingBytes);
    }

    #[test]
    fn decode_setup_rejects_truncated() {
        let bytes = encode_setup(&sample_setup_bundle());
        for cut in [0, 1, 4, 5, 10, bytes.len() - 1] {
            assert!(decode_setup(&bytes[..cut]).is_err(), "cut={cut} must be rejected");
        }
    }

    #[test]
    fn decode_setup_rejects_invalid_simple_params() {
        // Corrupt the first segment's lwe_dim (byte offset 25, right after
        // the 4-byte magic + 1 scheme byte + 20 bytes of CuckooParams+block)
        // to 0, which `SimpleParams::new` would otherwise panic on.
        let mut bytes = encode_setup(&sample_setup_bundle());
        let lwe_dim_offset = 4 + 1 + 5 * 4 + 8; // magic + scheme + 5 u32s + block u64
        bytes[lwe_dim_offset..lwe_dim_offset + 4].copy_from_slice(&0u32.to_le_bytes());
        assert_eq!(expect_setup_err(&bytes), WireError::InvalidSimpleParams { segment: 0 });
    }

    #[test]
    fn decode_setup_rejects_short_hint() {
        // Build a bundle by hand with a too-short hint on segment 0 (2
        // cells instead of the 4*6=24 the geometry below requires).
        let params = SimpleServerParams {
            params: SimpleParams::new(4, 8, 6.4, [0u8; 16]),
            n_rows: 10,
            row_width: 3,
            k: 2,
            reshape_rows: 5,
            reshape_row_width: 6,
        };
        let bundle = SetupBundle::<SimplePirBackend> {
            params: sample_cuckoo_params(),
            backend_params: vec![params.clone(), params.clone(), params],
            hints: vec![
                SimpleHint { data: vec![1, 2] }, // short: should be 24
                SimpleHint { data: vec![0; 24] },
                SimpleHint { data: vec![0; 24] },
            ],
            block: 0,
        };
        let bytes = encode_setup(&bundle);
        assert_eq!(
            expect_setup_err(&bytes),
            WireError::SegmentLengthMismatch { segment: 0, expected: 24, found: 2 }
        );
    }

    #[test]
    fn decode_query_bundle_rejects_arity_mismatch() {
        let queries = sample_queries();
        let bytes = encode_query_bundle(&queries);
        let params = CuckooParams {
            scheme_kind: SchemeKind::Segmented4ary, // arity 4, but the wire declares 3
            ..sample_cuckoo_params()
        };
        let expected_len_per_seg = vec![5u32; 4];
        assert_eq!(
            decode_query_bundle(&bytes, &params, &expected_len_per_seg).unwrap_err(),
            WireError::ArityMismatch { expected: 4, found: 3 }
        );
    }

    /// The load-bearing security check this module adds beyond a mere
    /// upper bound (see the module docs): a query segment shorter than the
    /// deployment's `reshape_rows` must be rejected here, not allowed
    /// through to `RisePirServer::answer`, which would index it
    /// out-of-bounds and panic.
    #[test]
    fn decode_query_bundle_rejects_too_short_segment() {
        let short_queries = vec![
            SimpleQuery { b: vec![1, 2] }, // only 2, geometry below expects 5
            SimpleQuery { b: vec![6, 7, 8, 9, 10] },
            SimpleQuery { b: vec![11, 12, 13, 14, 15] },
        ];
        let bytes = encode_query_bundle(&short_queries);
        let params = sample_cuckoo_params();
        let expected_len_per_seg = vec![5u32, 5, 5];
        assert_eq!(
            decode_query_bundle(&bytes, &params, &expected_len_per_seg).unwrap_err(),
            WireError::SegmentLengthMismatch { segment: 0, expected: 5, found: 2 }
        );
    }

    #[test]
    fn decode_query_bundle_rejects_trailing_bytes() {
        let mut bytes = encode_query_bundle(&sample_queries());
        bytes.push(0xAB);
        let params = sample_cuckoo_params();
        let expected_len_per_seg = vec![5u32, 5, 5];
        assert_eq!(
            decode_query_bundle(&bytes, &params, &expected_len_per_seg).unwrap_err(),
            WireError::TrailingBytes
        );
    }

    #[test]
    fn decode_response_bundle_rejects_arity_mismatch() {
        let responses = sample_responses();
        let bytes = encode_response_bundle(&responses, 7);
        let expected_len_per_seg = vec![3u32; 4];
        assert_eq!(
            decode_response_bundle(&bytes, &expected_len_per_seg, 4).unwrap_err(),
            WireError::ArityMismatch { expected: 4, found: 3 }
        );
    }

    /// Same load-bearing check as `decode_query_bundle_rejects_too_short_segment`,
    /// for the client-side response decode: `client_decode` slices its
    /// argument on the assumption `a.len() == reshape_row_width`.
    #[test]
    fn decode_response_bundle_rejects_too_short_segment() {
        let short_responses = vec![
            SimpleResponse { a: vec![100] }, // only 1, geometry below expects 3
            SimpleResponse { a: vec![400, 500, 600] },
            SimpleResponse { a: vec![700, 800, 900] },
        ];
        let bytes = encode_response_bundle(&short_responses, 99);
        let expected_len_per_seg = vec![3u32, 3, 3];
        assert_eq!(
            decode_response_bundle(&bytes, &expected_len_per_seg, 3).unwrap_err(),
            WireError::SegmentLengthMismatch { segment: 0, expected: 3, found: 1 }
        );
    }

    #[test]
    fn decode_response_bundle_rejects_trailing_bytes() {
        let mut bytes = encode_response_bundle(&sample_responses(), 1);
        bytes.push(0xCD);
        let expected_len_per_seg = vec![3u32, 3, 3];
        assert_eq!(
            decode_response_bundle(&bytes, &expected_len_per_seg, 3).unwrap_err(),
            WireError::TrailingBytes
        );
    }

    // ── panic-safety fuzzing: random bytes, truncations, bit flips ─────
    //
    // Exactly the three-pronged approach `risepir_proto::codec`'s own
    // `fuzz_no_panic_on_random_bytes` /
    // `fuzz_no_panic_on_truncated_and_corrupted_valid_encodings` use,
    // applied to every decoder in this module.

    #[test]
    fn fuzz_decode_setup_no_panic() {
        use std::panic;
        let mut state: u64 = 0x1234_5678_9ABC_DEF1;
        for len in 0..300usize {
            let bytes = random_bytes(&mut state, len);
            let result = panic::catch_unwind(|| decode_setup(&bytes));
            assert!(result.is_ok(), "panicked on random input of length {len}");
        }

        let valid = encode_setup(&sample_setup_bundle());
        for cut in 0..=valid.len() {
            let prefix = valid[..cut].to_vec();
            let result = panic::catch_unwind(|| decode_setup(&prefix));
            assert!(result.is_ok(), "panicked truncating to length {cut}");
        }
        for byte_idx in 0..valid.len() {
            for bit in 0..8u8 {
                let mut corrupted = valid.clone();
                corrupted[byte_idx] ^= 1 << bit;
                let result = panic::catch_unwind(|| decode_setup(&corrupted));
                assert!(result.is_ok(), "panicked corrupting byte {byte_idx} bit {bit}");
            }
        }
    }

    #[test]
    fn fuzz_decode_query_bundle_no_panic() {
        use std::panic;
        let params = sample_cuckoo_params();
        let expected_len_per_seg = vec![5u32, 5, 5];

        let mut state: u64 = 0x0BAD_F00D_CAFE_BABE;
        for len in 0..300usize {
            let bytes = random_bytes(&mut state, len);
            let result = panic::catch_unwind(|| decode_query_bundle(&bytes, &params, &expected_len_per_seg));
            assert!(result.is_ok(), "panicked on random input of length {len}");
        }

        let valid = encode_query_bundle(&sample_queries());
        for cut in 0..=valid.len() {
            let prefix = valid[..cut].to_vec();
            let result = panic::catch_unwind(|| decode_query_bundle(&prefix, &params, &expected_len_per_seg));
            assert!(result.is_ok(), "panicked truncating to length {cut}");
        }
        for byte_idx in 0..valid.len() {
            for bit in 0..8u8 {
                let mut corrupted = valid.clone();
                corrupted[byte_idx] ^= 1 << bit;
                let result = panic::catch_unwind(|| decode_query_bundle(&corrupted, &params, &expected_len_per_seg));
                assert!(result.is_ok(), "panicked corrupting byte {byte_idx} bit {bit}");
            }
        }
    }

    #[test]
    fn fuzz_decode_response_bundle_no_panic() {
        use std::panic;
        let expected_len_per_seg = vec![3u32, 3, 3];

        let mut state: u64 = 0x5EED_F00D_1234_5678;
        for len in 0..300usize {
            let bytes = random_bytes(&mut state, len);
            let result = panic::catch_unwind(|| decode_response_bundle(&bytes, &expected_len_per_seg, 3));
            assert!(result.is_ok(), "panicked on random input of length {len}");
        }

        let valid = encode_response_bundle(&sample_responses(), 555);
        for cut in 0..=valid.len() {
            let prefix = valid[..cut].to_vec();
            let result = panic::catch_unwind(|| decode_response_bundle(&prefix, &expected_len_per_seg, 3));
            assert!(result.is_ok(), "panicked truncating to length {cut}");
        }
        for byte_idx in 0..valid.len() {
            for bit in 0..8u8 {
                let mut corrupted = valid.clone();
                corrupted[byte_idx] ^= 1 << bit;
                let result = panic::catch_unwind(|| decode_response_bundle(&corrupted, &expected_len_per_seg, 3));
                assert!(result.is_ok(), "panicked corrupting byte {byte_idx} bit {bit}");
            }
        }
    }
}
