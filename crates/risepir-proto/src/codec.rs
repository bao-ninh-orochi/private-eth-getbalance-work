//! Wire codec for [`crate::types::BlockDelta`] and for the `Vec<u32>`
//! payloads the PIR query/response/hint types are built from.
//!
//! # Purpose
//!
//! `ikpir_common::wire` deliberately ships no serialization (`wire.rs:13-18`
//! there says so explicitly) — bundles are plain in-process data, and
//! deployments are expected to layer their own wire format on top. This
//! module is that layer for `risepir-proto`'s own types.
//!
//! # `BlockDelta` compaction
//!
//! `docs/verification.md` proves a coalesced delta telescopes:
//! `sum(c_{i+1} - c_i) = c_final - c_initial`, and both endpoints are real
//! cell states in `[0, p)`, so **`|delta| < p <= 2^14` no matter how many
//! blocks were folded into it** (upstream's `u16` offset + `i64` delta costs
//! 10 bytes/cell to carry that). This codec instead uses `varint(offset_gap)
//! + zigzag_varint(delta)` (offsets are sorted ascending, so gaps are
//! small), and turns the magnitude bound into a free integrity check:
//! [`decode_block_delta`] rejects any cell with `|delta| >= 2^plaintext_bits`.
//!
//! # Untrusted-input discipline
//!
//! A PIR server ingests attacker-controlled, multi-hundred-KB blobs over
//! this codec. Every count read from the wire (segment row count, per-row
//! cell count, `Vec<u32>` length) is validated against the number of bytes
//! actually remaining in the input **before** it is used to size a `Vec`
//! allocation. Concretely: since every encoded item costs at least one
//! byte, a declared count that exceeds the remaining input length can never
//! be satisfied, so capping each count at `remaining_bytes.len()` is a
//! correct — not merely heuristic — bound. This is the "passed-in max": the
//! byte slice the caller hands in is itself the ceiling, with no separate
//! configuration needed. `decode_u32s` additionally takes an explicit
//! `max_len` from the caller, since there the caller (server/client) always
//! knows the exact expected length from its own [`crate::geometry::Sizes`]
//! and a tighter bound than "however many 4-byte groups fit" is available.
//!
//! Malformed input — truncated, corrupted, or adversarial — always produces
//! an `Err`, never a panic and never an attempted large allocation.

use crate::types::{BlockDelta, SegmentRowDeltas};

const MAGIC: [u8; 4] = *b"RPD1";

/// Errors from [`decode_block_delta`] / [`decode_u32s`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CodecError {
    /// Input is shorter than the 4-byte magic, or the magic bytes present
    /// are not `b"RPD1"`.
    BadMagic,
    /// Input ended before a fixed-size field or a declared count of items
    /// could be fully read.
    UnexpectedEof,
    /// A varint was malformed: more than 10 continuation bytes, or it
    /// encodes a value that does not fit `u64`.
    MalformedVarint,
    /// The header's `arity` byte did not match the caller-supplied expected
    /// arity.
    ArityMismatch {
        /// Arity the caller expected.
        expected: u32,
        /// Arity the header declared.
        found: u32,
    },
    /// A declared count (row count, cell count, or `Vec<u32>` length)
    /// exceeded the number of bytes remaining in the input, and was
    /// therefore rejected before being used to size an allocation.
    CountExceedsInput,
    /// A reconstructed row index or cell offset overflowed its type
    /// (`u32` / `u16` respectively) while accumulating gaps.
    IndexOverflow,
    /// A cell's `|delta| >= 2^plaintext_bits` — the free integrity check
    /// licensed by the telescoping bound (see the module docs). Also
    /// returned if `plaintext_bits` itself is outside `1..=31` (the same
    /// range `ikpir_common`'s LWE param constructors enforce), since the
    /// bound is meaningless outside it.
    DeltaOutOfRange,
    /// Bytes remained in the input after a complete, well-formed message
    /// was parsed. Treated as corruption rather than silently ignored.
    TrailingBytes,
}

impl std::fmt::Display for CodecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadMagic => write!(f, "bad magic: not a RPD1 block delta"),
            Self::UnexpectedEof => write!(f, "unexpected end of input"),
            Self::MalformedVarint => write!(f, "malformed varint"),
            Self::ArityMismatch { expected, found } => {
                write!(f, "arity mismatch: expected {expected}, header declared {found}")
            }
            Self::CountExceedsInput => write!(f, "declared count exceeds remaining input length"),
            Self::IndexOverflow => write!(f, "reconstructed row/offset index overflowed"),
            Self::DeltaOutOfRange => write!(f, "delta out of range for plaintext_bits"),
            Self::TrailingBytes => write!(f, "trailing bytes after a complete message"),
        }
    }
}

impl std::error::Error for CodecError {}

// ─── varint / zigzag primitives ─────────────────────────────────────────

fn write_uvarint(out: &mut Vec<u8>, mut value: u64) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

/// Reads one LEB128 unsigned varint starting at `*pos`, advancing `*pos`
/// past it. Rejects encodings longer than 10 bytes and encodings whose
/// value does not fit `u64` — both would otherwise require an unbounded
/// shift amount, which is exactly the kind of malformed input this codec
/// must never panic on.
fn read_uvarint(bytes: &[u8], pos: &mut usize) -> Result<u64, CodecError> {
    let mut result: u64 = 0;
    let mut shift: u32 = 0;
    loop {
        if shift >= 64 {
            return Err(CodecError::MalformedVarint);
        }
        let byte = *bytes.get(*pos).ok_or(CodecError::UnexpectedEof)?;
        *pos += 1;
        let low7 = u64::from(byte & 0x7f);
        let shifted = low7.wrapping_shl(shift);
        if (shifted >> shift) != low7 {
            // Bits fell off the top: this varint does not fit in u64.
            return Err(CodecError::MalformedVarint);
        }
        result |= shifted;
        if byte & 0x80 == 0 {
            return Ok(result);
        }
        shift += 7;
    }
}

fn zigzag_encode(v: i64) -> u64 {
    ((v << 1) ^ (v >> 63)) as u64
}

fn zigzag_decode(u: u64) -> i64 {
    ((u >> 1) as i64) ^ -((u & 1) as i64)
}

fn uvarint_len(mut value: u64) -> usize {
    let mut n = 1;
    while value >= 0x80 {
        value >>= 7;
        n += 1;
    }
    n
}

/// Caps `count` at the number of bytes remaining in the input — see the
/// module-level "untrusted-input discipline" note. Returns `count` as a
/// `usize`, which is always safe once this check passes.
fn check_count(count: u64, remaining: usize) -> Result<usize, CodecError> {
    if count > remaining as u64 {
        return Err(CodecError::CountExceedsInput);
    }
    Ok(count as usize)
}

// ─── BlockDelta ──────────────────────────────────────────────────────────

/// Encodes `delta` as: magic `b"RPD1"`, `block: u64` LE, `arity: u8`, then
/// per segment `varint(row_count)` and per row `varint(row_gap)` (rows
/// taken in ascending order, delta-encoded from a running `0`) +
/// `varint(cell_count)` and per cell `varint(offset_gap)` (offsets taken in
/// ascending order within the row, same delta-encoding) +
/// `zigzag_varint(delta)`.
///
/// Rows within a segment and offsets within a row are sorted ascending
/// before encoding — `delta` need not already be canonicalized (though the
/// output of [`BlockDelta::coalesce`](crate::types::BlockDelta::coalesce)
/// already is).
///
/// `plaintext_bits` is used only for a `debug_assert` that every delta
/// already satisfies the magnitude bound this format relies on
/// (`docs/verification.md`) — it does not change the bytes produced, so it
/// is a no-op in release builds.
///
/// # Panics
///
/// If `delta.per_segment.len() > 255` (the wire arity byte is `u8`-width;
/// real deployments use arity 2, 3, or 4, so this indicates a caller bug,
/// not adversarial input — `encode_block_delta` runs on locally-constructed
/// data, never directly on untrusted bytes).
pub fn encode_block_delta(delta: &BlockDelta, plaintext_bits: u32) -> Vec<u8> {
    assert!(
        delta.per_segment.len() <= u8::MAX as usize,
        "encode_block_delta: {} segments does not fit the u8 arity byte",
        delta.per_segment.len()
    );

    let mut out = Vec::new();
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&delta.block.to_le_bytes());
    out.push(delta.per_segment.len() as u8);

    for seg in &delta.per_segment {
        let mut rows: Vec<&(u32, Vec<(u16, i64)>)> = seg.iter().collect();
        rows.sort_by_key(|(row, _)| *row);
        write_uvarint(&mut out, rows.len() as u64);

        let mut prev_row = 0u32;
        for (row, cells) in rows {
            write_uvarint(&mut out, u64::from(row - prev_row));
            prev_row = *row;

            let mut sorted_cells: Vec<&(u16, i64)> = cells.iter().collect();
            sorted_cells.sort_by_key(|(offset, _)| *offset);
            write_uvarint(&mut out, sorted_cells.len() as u64);

            let mut prev_off = 0u32;
            for (offset, cell_delta) in sorted_cells {
                debug_assert!(
                    (1..=31).contains(&plaintext_bits),
                    "encode_block_delta: plaintext_bits out of range"
                );
                debug_assert!(
                    cell_delta.unsigned_abs() < (1u64 << plaintext_bits),
                    "encode_block_delta: delta out of range for plaintext_bits; \
                     the coalesced-delta magnitude invariant was violated upstream"
                );
                write_uvarint(&mut out, u64::from(u32::from(*offset) - prev_off));
                prev_off = u32::from(*offset);
                write_uvarint(&mut out, zigzag_encode(*cell_delta));
            }
        }
    }
    out
}

/// Inverse of [`encode_block_delta`]. `arity` is the caller's expected
/// segment count; a header that declares a different arity is rejected
/// with [`CodecError::ArityMismatch`] rather than trusted.
///
/// # Errors
///
/// See [`CodecError`]. In particular: [`CodecError::DeltaOutOfRange`] if
/// any decoded `|delta| >= 2^plaintext_bits`, and [`CodecError::TrailingBytes`]
/// if the input has bytes left over after a complete message is parsed —
/// both are the format's two integrity checks (magnitude bound, exact
/// framing) firing as designed rather than silently accepting corruption.
///
/// Never panics, regardless of input content: every count is validated
/// against the remaining input length before it is used to size an
/// allocation, and every arithmetic reconstruction is checked.
pub fn decode_block_delta(bytes: &[u8], plaintext_bits: u32, arity: u32) -> Result<BlockDelta, CodecError> {
    if !(1..=31).contains(&plaintext_bits) {
        return Err(CodecError::DeltaOutOfRange);
    }
    // u64, not i64: `delta.unsigned_abs()` on `delta == i64::MIN` is
    // `2^63`, which does not fit back in `i64` — comparing in `u64`
    // avoids the wraparound that would otherwise let that one value
    // slip past the range check.
    let bound: u64 = 1u64 << plaintext_bits;

    if bytes.len() < 4 || bytes[0..4] != MAGIC {
        return Err(CodecError::BadMagic);
    }
    let mut pos = 4usize;

    let block_bytes: [u8; 8] = bytes
        .get(pos..pos + 8)
        .ok_or(CodecError::UnexpectedEof)?
        .try_into()
        .expect("slice of length 8");
    let block = u64::from_le_bytes(block_bytes);
    pos += 8;

    let arity_byte = *bytes.get(pos).ok_or(CodecError::UnexpectedEof)?;
    pos += 1;
    if u32::from(arity_byte) != arity {
        return Err(CodecError::ArityMismatch {
            expected: arity,
            found: u32::from(arity_byte),
        });
    }

    let mut per_segment: Vec<SegmentRowDeltas> = Vec::with_capacity(arity as usize);
    for _ in 0..arity {
        let row_count_raw = read_uvarint(bytes, &mut pos)?;
        let row_count = check_count(row_count_raw, bytes.len() - pos)?;

        let mut rows: SegmentRowDeltas = Vec::with_capacity(row_count);
        let mut prev_row: u64 = 0;
        for _ in 0..row_count {
            let row_gap = read_uvarint(bytes, &mut pos)?;
            let row = prev_row.checked_add(row_gap).ok_or(CodecError::IndexOverflow)?;
            let row_u32 = u32::try_from(row).map_err(|_| CodecError::IndexOverflow)?;
            prev_row = row;

            let cell_count_raw = read_uvarint(bytes, &mut pos)?;
            let cell_count = check_count(cell_count_raw, bytes.len() - pos)?;

            let mut cells: Vec<(u16, i64)> = Vec::with_capacity(cell_count);
            let mut prev_off: u64 = 0;
            for _ in 0..cell_count {
                let off_gap = read_uvarint(bytes, &mut pos)?;
                let offset = prev_off.checked_add(off_gap).ok_or(CodecError::IndexOverflow)?;
                let offset_u16 = u16::try_from(offset).map_err(|_| CodecError::IndexOverflow)?;
                prev_off = offset;

                let delta_raw = read_uvarint(bytes, &mut pos)?;
                let delta = zigzag_decode(delta_raw);
                if delta.unsigned_abs() >= bound {
                    return Err(CodecError::DeltaOutOfRange);
                }
                cells.push((offset_u16, delta));
            }
            rows.push((row_u32, cells));
        }
        per_segment.push(rows);
    }

    if pos != bytes.len() {
        return Err(CodecError::TrailingBytes);
    }

    Ok(BlockDelta { block, per_segment })
}

impl BlockDelta {
    /// Exact byte length [`encode_block_delta`] would produce for this
    /// delta, computed without allocating the output buffer. For comparing
    /// against the 10-bytes/cell upstream baseline in benchmarks.
    pub fn encoded_len(&self) -> usize {
        let mut len = 4 + 8 + 1; // magic + block + arity
        for seg in &self.per_segment {
            len += uvarint_len(seg.len() as u64);
            let mut rows: Vec<&(u32, Vec<(u16, i64)>)> = seg.iter().collect();
            rows.sort_by_key(|(row, _)| *row);
            let mut prev_row = 0u32;
            for (row, cells) in rows {
                len += uvarint_len(u64::from(row - prev_row));
                prev_row = *row;
                len += uvarint_len(cells.len() as u64);
                let mut sorted_cells: Vec<&(u16, i64)> = cells.iter().collect();
                sorted_cells.sort_by_key(|(offset, _)| *offset);
                let mut prev_off = 0u32;
                for (offset, cell_delta) in sorted_cells {
                    len += uvarint_len(u64::from(u32::from(*offset) - prev_off));
                    prev_off = u32::from(*offset);
                    len += uvarint_len(zigzag_encode(*cell_delta));
                }
            }
        }
        len
    }
}

// ─── Vec<u32> length-prefixed codec ─────────────────────────────────────

/// Encodes `values` as `varint(values.len())` followed by `values.len()`
/// little-endian `u32`s. For the PIR query/response/hint payloads, which
/// are all `Vec<u32>` upstream with no wire format of their own
/// (`ikpir_common::wire.rs:13-18`).
pub fn encode_u32s(values: &[u32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(5 + values.len() * 4);
    write_uvarint(&mut out, values.len() as u64);
    for v in values {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out
}

/// Inverse of [`encode_u32s`]. `max_len` is a caller-supplied ceiling on
/// the decoded length (e.g. the segment's expected query/response/hint
/// length from [`crate::geometry::Sizes`]) — checked *before* the declared
/// length is used to size an allocation, on top of the always-on
/// remaining-bytes bound (see the module docs).
///
/// # Errors
///
/// [`CodecError::CountExceedsInput`] if the declared length exceeds
/// `max_len` or the input cannot possibly hold that many `u32`s.
/// [`CodecError::UnexpectedEof`] on a truncated final `u32`.
/// [`CodecError::TrailingBytes`] if bytes remain after the declared count
/// is fully read.
pub fn decode_u32s(bytes: &[u8], max_len: usize) -> Result<Vec<u32>, CodecError> {
    let mut pos = 0usize;
    let len_raw = read_uvarint(bytes, &mut pos)?;
    if len_raw > max_len as u64 {
        return Err(CodecError::CountExceedsInput);
    }
    let remaining = bytes.len() - pos;
    if len_raw > (remaining as u64) / 4 {
        return Err(CodecError::CountExceedsInput);
    }
    let len = len_raw as usize;

    let mut out = Vec::with_capacity(len);
    for _ in 0..len {
        let chunk: [u8; 4] = bytes
            .get(pos..pos + 4)
            .ok_or(CodecError::UnexpectedEof)?
            .try_into()
            .expect("slice of length 4");
        out.push(u32::from_le_bytes(chunk));
        pos += 4;
    }
    if pos != bytes.len() {
        return Err(CodecError::TrailingBytes);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::BlockDelta;

    fn sample_delta() -> BlockDelta {
        BlockDelta {
            block: 424_242,
            per_segment: vec![
                vec![(5, vec![(3, -100), (10, 7)]), (1, vec![(0, 1)])],
                vec![],
                vec![(2_000_000, vec![(65_535, -1)])],
            ],
        }
    }

    #[test]
    fn round_trip_basic() {
        let delta = sample_delta();
        let bytes = encode_block_delta(&delta, 14);
        assert_eq!(bytes.len(), delta.encoded_len());
        let decoded = decode_block_delta(&bytes, 14, 3).unwrap();
        // Rows/offsets come back sorted, matching coalesce()'s canonical form.
        let mut expected = delta.clone();
        for seg in &mut expected.per_segment {
            seg.sort_by_key(|(row, _)| *row);
            for (_, cells) in seg.iter_mut() {
                cells.sort_by_key(|(off, _)| *off);
            }
        }
        assert_eq!(decoded, expected);
    }

    #[test]
    fn round_trip_empty_delta() {
        let delta = BlockDelta {
            block: 0,
            per_segment: vec![vec![], vec![], vec![], vec![]],
        };
        let bytes = encode_block_delta(&delta, 10);
        assert_eq!(bytes.len(), delta.encoded_len());
        let decoded = decode_block_delta(&bytes, 10, 4).unwrap();
        assert_eq!(decoded, delta);
    }

    #[test]
    fn beats_ten_bytes_per_cell_baseline() {
        // Ten cells with small deltas and small offset gaps: this is
        // exactly the regime the module doc claims ~3 B/cell for.
        let cells: Vec<(u16, i64)> = (0..10).map(|i| (i * 2, 5)).collect();
        let delta = BlockDelta {
            block: 1,
            per_segment: vec![vec![(0, cells)]],
        };
        let bytes = encode_block_delta(&delta, 10);
        let naive_baseline = 10 * 10; // upstream: 10 B/cell
        assert!(
            bytes.len() < naive_baseline,
            "encoded {} bytes, naive baseline {naive_baseline}",
            bytes.len()
        );
    }

    #[test]
    fn decode_rejects_bad_magic() {
        let mut bytes = encode_block_delta(&sample_delta(), 14);
        bytes[0] = b'X';
        assert_eq!(decode_block_delta(&bytes, 14, 3), Err(CodecError::BadMagic));
    }

    #[test]
    fn decode_rejects_short_input() {
        assert_eq!(decode_block_delta(&[], 14, 3), Err(CodecError::BadMagic));
        assert_eq!(decode_block_delta(b"RPD1", 14, 3), Err(CodecError::UnexpectedEof));
    }

    #[test]
    fn decode_rejects_arity_mismatch() {
        let bytes = encode_block_delta(&sample_delta(), 14);
        assert_eq!(
            decode_block_delta(&bytes, 14, 4),
            Err(CodecError::ArityMismatch { expected: 4, found: 3 })
        );
    }

    #[test]
    fn decode_rejects_delta_out_of_range() {
        // plaintext_bits = 4 => bound = 16; sample_delta has a delta of -100.
        let bytes = encode_block_delta(&sample_delta(), 30);
        assert_eq!(decode_block_delta(&bytes, 4, 3), Err(CodecError::DeltaOutOfRange));
    }

    #[test]
    fn decode_rejects_invalid_plaintext_bits() {
        let bytes = encode_block_delta(&sample_delta(), 14);
        assert_eq!(decode_block_delta(&bytes, 0, 3), Err(CodecError::DeltaOutOfRange));
        assert_eq!(decode_block_delta(&bytes, 32, 3), Err(CodecError::DeltaOutOfRange));
    }

    #[test]
    fn decode_rejects_trailing_bytes() {
        let mut bytes = encode_block_delta(&sample_delta(), 14);
        bytes.push(0xff);
        assert_eq!(decode_block_delta(&bytes, 14, 3), Err(CodecError::TrailingBytes));
    }

    #[test]
    fn decode_rejects_truncated_valid_encoding() {
        let bytes = encode_block_delta(&sample_delta(), 14);
        // Truncate strictly inside the cell-data region: guaranteed incomplete.
        let cut = bytes.len() - 2;
        assert!(decode_block_delta(&bytes[..cut], 14, 3).is_err());
    }

    #[test]
    fn decode_rejects_huge_declared_count_without_allocating() {
        // A header claiming an enormous row_count backed by almost no data
        // must be rejected via the length check, not attempted.
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&MAGIC);
        bytes.extend_from_slice(&1u64.to_le_bytes());
        bytes.push(1); // arity = 1
        write_uvarint(&mut bytes, u64::MAX); // row_count
        assert_eq!(
            decode_block_delta(&bytes, 14, 1),
            Err(CodecError::CountExceedsInput)
        );
    }

    #[test]
    fn fuzz_no_panic_on_random_bytes() {
        use std::panic;
        let mut state: u64 = 0x2545_F491_4F6C_DD1D;
        let mut next = move || {
            // xorshift64*, deterministic and dependency-free.
            state ^= state >> 12;
            state ^= state << 25;
            state ^= state >> 27;
            state.wrapping_mul(0x2545_F491_4F6C_DD1D)
        };
        for len in 0..300usize {
            let bytes: Vec<u8> = (0..len).map(|_| (next() % 256) as u8).collect();
            let result = panic::catch_unwind(|| decode_block_delta(&bytes, 14, 3));
            assert!(result.is_ok(), "panicked on random input of length {len}: {bytes:?}");
        }
    }

    #[test]
    fn fuzz_no_panic_on_truncated_and_corrupted_valid_encodings() {
        use std::panic;
        let valid = encode_block_delta(&sample_delta(), 14);
        for cut in 0..=valid.len() {
            let prefix = valid[..cut].to_vec();
            let result = panic::catch_unwind(|| decode_block_delta(&prefix, 14, 3));
            assert!(result.is_ok(), "panicked truncating to length {cut}");
        }
        for byte_idx in 0..valid.len() {
            for bit in 0..8u8 {
                let mut corrupted = valid.clone();
                corrupted[byte_idx] ^= 1 << bit;
                let result = panic::catch_unwind(|| decode_block_delta(&corrupted, 14, 3));
                assert!(result.is_ok(), "panicked corrupting byte {byte_idx} bit {bit}");
            }
        }
    }

    #[test]
    fn u32s_round_trip() {
        let values: Vec<u32> = vec![0, 1, u32::MAX, 12345, 999_999_999];
        let bytes = encode_u32s(&values);
        assert_eq!(decode_u32s(&bytes, values.len()).unwrap(), values);
    }

    #[test]
    fn u32s_empty_round_trip() {
        let bytes = encode_u32s(&[]);
        assert_eq!(decode_u32s(&bytes, 0).unwrap(), Vec::<u32>::new());
    }

    #[test]
    fn u32s_rejects_over_max_len() {
        let values: Vec<u32> = vec![1, 2, 3];
        let bytes = encode_u32s(&values);
        assert_eq!(decode_u32s(&bytes, 2), Err(CodecError::CountExceedsInput));
    }

    #[test]
    fn u32s_rejects_huge_declared_len_without_allocating() {
        let mut bytes = Vec::new();
        write_uvarint(&mut bytes, u64::MAX);
        assert_eq!(
            decode_u32s(&bytes, usize::MAX),
            Err(CodecError::CountExceedsInput)
        );
    }

    #[test]
    fn u32s_rejects_trailing_bytes() {
        let mut bytes = encode_u32s(&[1, 2, 3]);
        bytes.push(0);
        assert_eq!(decode_u32s(&bytes, 10), Err(CodecError::TrailingBytes));
    }

    #[test]
    fn u32s_fuzz_no_panic() {
        use std::panic;
        let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut next = move || {
            state ^= state >> 12;
            state ^= state << 25;
            state ^= state >> 27;
            state.wrapping_mul(0x2545_F491_4F6C_DD1D)
        };
        for len in 0..200usize {
            let bytes: Vec<u8> = (0..len).map(|_| (next() % 256) as u8).collect();
            let result = panic::catch_unwind(|| decode_u32s(&bytes, 1_000_000));
            assert!(result.is_ok(), "panicked on random input of length {len}");
        }
    }

    proptest::proptest! {
        #[test]
        fn block_delta_round_trip_prop(
            block in proptest::num::u64::ANY,
            seg_data in proptest::collection::vec(
                proptest::collection::btree_map(
                    0u32..1000,
                    proptest::collection::btree_map(0u16..1000, -8000i64..8000, 0..6),
                    0..8,
                ),
                1..4,
            ),
        ) {
            let per_segment: Vec<SegmentRowDeltas> = seg_data
                .into_iter()
                .map(|rows| {
                    rows.into_iter()
                        .filter(|(_, cells)| !cells.is_empty())
                        .map(|(row, cells)| (row, cells.into_iter().collect()))
                        .collect()
                })
                .collect();
            let delta = BlockDelta { block, per_segment };
            let bytes = encode_block_delta(&delta, 14);
            proptest::prop_assert_eq!(bytes.len(), delta.encoded_len());
            let decoded = decode_block_delta(&bytes, 14, delta.per_segment.len() as u32).unwrap();
            proptest::prop_assert_eq!(decoded, delta);
        }

        #[test]
        fn u32s_round_trip_prop(values in proptest::collection::vec(proptest::num::u32::ANY, 0..200)) {
            let bytes = encode_u32s(&values);
            let decoded = decode_u32s(&bytes, values.len())?;
            proptest::prop_assert_eq!(decoded, values);
        }

        #[test]
        fn decode_block_delta_never_panics_prop(bytes in proptest::collection::vec(proptest::num::u8::ANY, 0..500)) {
            let _ = decode_block_delta(&bytes, 14, 3);
        }
    }
}
