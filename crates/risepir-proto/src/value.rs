//! The value encoding: `key_tag ‖ balance ‖ checksum`, packed into
//! `value_bits` bits (`docs/plan.md` §3.5, ADR-0009).
//!
//! # Purpose
//!
//! This is the "never return a wrong answer" surface named in
//! `docs/plan.md`'s enforcement table, and it closes three
//! silent-wrong-answer paths at once:
//!
//! 1. **False positives.** The Segmented Cuckoo Filter's own positioning
//!    fingerprint is only 32 bits (`fingerprint_bits`, unchanged upstream)
//!    — a query for a nonexistent account can land on a slot whose
//!    fingerprint happens to collide, at a ~2⁻²⁸ rate. [`ValueCodec::key_tag`]
//!    is a *second*, independently-seeded 32-bit-class hash of the same
//!    address, carried inside the value bytes (which are opaque to the
//!    cuckoo store). Requiring both to agree extends the effective
//!    fingerprint to 64 bits (~2⁻⁶⁰) with **no upstream change** — see
//!    ADR-0009 for why this beats widening the store's own fingerprint
//!    type.
//! 2. **Balance corruption.** LWE decode noise can corrupt a value cell
//!    while the fingerprint (and `key_tag`) cells stay intact, in which
//!    case an unprotected scheme would return a *plausible, silently
//!    wrong* balance. The checksum turns that into a loud
//!    [`Lookup::DecodeFailed`] instead.
//! 3. **Overflow.** [`ValueCodec::encode`] hard-fails
//!    ([`ValueError::BalanceOverflow`]) rather than truncating a balance
//!    that does not fit `balance_bits` — silent truncation is exactly the
//!    kind of wrong answer this module exists to prevent.
//!
//! # Why `key_tag` and `checksum` stay separate fields
//!
//! Keeping them separate (rather than one combined tag over
//! `address ‖ balance`) is what preserves the [`Lookup::NotFound`] vs.
//! [`Lookup::DecodeFailed`] distinction: a `key_tag` mismatch means "this
//! slot is not our key, keep scanning" (→ `NotFound` if no slot matches),
//! while a `checksum` mismatch on a `key_tag`-matching slot means "this
//! *is* our key but its balance decoded corrupt" (→ `DecodeFailed`,
//! never silently `0x0`).

use xxhash_rust::xxh3::xxh3_64_with_seed;

use crate::types::AddressHash;

/// A balance in wei — re-exported at crate root as `risepir_proto::Balance`;
/// imported here to give [`ValueCodec::encode`] / [`ValueCodec::decode`]
/// concrete signatures.
pub type Balance = crate::types::Balance;

/// Outcome of decoding a candidate slot's value bytes against a claimed
/// address: [`ValueCodec::decode`] returns this directly, and
/// `risepir-client`'s `RisePirClient::finish` (which scans every
/// candidate slot across every segment) folds its own scan down to the
/// same three-way outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lookup {
    /// A slot carried a matching `key_tag` for the queried address, and
    /// its balance decoded with a matching checksum.
    Found(Balance),
    /// No scanned slot carried a `key_tag` matching the queried address.
    /// Per ADR-0015, callers serving a *complete* nonzero-balance
    /// database may treat this as balance zero — that policy decision is
    /// deliberately not made in this crate.
    NotFound,
    /// A slot's `key_tag` matched the queried address, but the decoded
    /// balance's checksum did not (ADR-0009: LWE noise corrupted a value
    /// cell) — or, only from [`ValueCodec::decode`] directly, the codec
    /// itself was constructed with unrepresentable widths (a construction
    /// bug; see that method's docs). Never a number — callers must treat
    /// this as a failed read, never a balance of `0` or any other value.
    DecodeFailed,
}

/// Seed for [`ValueCodec::key_tag`]'s hash. **Must be nonzero** and must
/// differ from the Segmented Cuckoo Filter's own positioning fingerprint,
/// which is the low bits of `xxh3_64(address)` — i.e. `xxh3_64_with_seed`
/// with an *implicit* seed of `0`
/// (`segmented_cuckoo::hash::hash_and_i1`/`extract_fingerprint`). Using a
/// distinct nonzero seed here — rather than seed `0` again — is what makes
/// the value's `key_tag` statistically independent of the store's own
/// fingerprint even though both hash the exact same 32 address bytes; that
/// independence is what lets the two 32-bit-class tags combine into a
/// 64-bit-effective fingerprint (ADR-0009) instead of just checking the
/// same 32 bits twice. Value has no other significance; any fixed nonzero
/// `u64` works, this one spells "RISE_TAG" in ASCII.
const KEY_TAG_SEED: u64 = 0x5249_5345_5f54_4147;

/// Packs/unpacks `key_tag ‖ balance ‖ checksum` as a little-endian
/// bitstring of [`Self::value_bits`] bits, stored in `ceil(value_bits / 8)`
/// bytes. `key_tag` occupies the *low* bits (written/read first) so client
/// extraction ([`Self::extract_key_tag`]) never has to skip past the
/// (possibly much wider) balance field.
///
/// # Checksum function
///
/// FNV-1a-64 (offset basis `0xcbf29ce484222325`, prime `0x100000001b3`) over
/// `balance`'s little-endian bytes, truncated to the low `balance_bits` bits
/// (i.e. hashed over exactly `ceil(balance_bits / 8)` bytes — no more, no
/// fewer), then truncated to the low `checksum_bits` bits of the resulting
/// 64-bit hash. Deterministic; no keying, no randomness; independent of
/// `key_tag` (a corrupted balance is still caught even if, hypothetically,
/// the address were somehow wrong too).
///
/// # Constraints
///
/// `key_tag_bits <= 64` (must fit a `u64`), `balance_bits <= 128` (must fit
/// [`Balance`]), and `checksum_bits <= 64` (must fit the FNV-1a-64 output);
/// `encode` returns [`ValueError::InvalidWidth`] otherwise, and `decode`
/// treats it as [`Lookup::DecodeFailed`] (see that method's docs).
/// `key_tag_bits == 0` and/or `checksum_bits == 0` are supported and
/// disable the respective check (every value then trivially satisfies it)
/// — this exists so each tag's cost can be measured against not carrying
/// it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ValueCodec {
    /// Width of the `key_tag` field in bits. `0` disables the check (every
    /// slot's `key_tag` is trivially `0` and matches).
    pub key_tag_bits: u32,
    /// Width of the balance field in bits.
    pub balance_bits: u32,
    /// Width of the checksum field in bits. `0` disables the checksum.
    pub checksum_bits: u32,
}

/// Errors from [`ValueCodec::encode`]. `decode` is total — see its own
/// docs — and never returns this type; its analogous failure modes are
/// [`Lookup::NotFound`] / [`Lookup::DecodeFailed`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ValueError {
    /// `encode` was asked to pack a `balance >= 2^balance_bits`. The
    /// balance is never truncated to fit — this is a hard failure by
    /// design (`docs/plan.md` ADR-0009).
    BalanceOverflow,
    /// `key_tag_bits > 64`, `balance_bits > 128`, or `checksum_bits > 64`,
    /// which this codec cannot represent (see the struct-level
    /// constraints note).
    InvalidWidth,
}

impl std::fmt::Display for ValueError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BalanceOverflow => write!(f, "balance does not fit in balance_bits"),
            Self::InvalidWidth => {
                write!(
                    f,
                    "key_tag_bits must be <= 64, balance_bits must be <= 128, and checksum_bits must be <= 64"
                )
            }
        }
    }
}

impl std::error::Error for ValueError {}

const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash = FNV_OFFSET_BASIS;
    for &b in bytes {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

/// Appends the low `num_bits` bits of `value` (`num_bits <= 128`) to `buf`
/// starting at `*bit_pos`, treating `buf` as one contiguous little-endian
/// bitstring (bit `0` of the logical value is bit `0` of `buf[0]`; bit `8`
/// is bit `0` of `buf[1]`; etc). Advances `*bit_pos` by `num_bits`.
///
/// # Constraints
///
/// `buf` must have at least `ceil((*bit_pos + num_bits) / 8)` bytes —
/// callers in this module always size `buf` from `value_bits` up front, so
/// this is a precondition, not a runtime check.
fn write_bits(buf: &mut [u8], bit_pos: &mut usize, value: u128, num_bits: u32) {
    for i in 0..num_bits {
        if (value >> i) & 1 == 1 {
            let byte_idx = *bit_pos / 8;
            let bit_idx = *bit_pos % 8;
            buf[byte_idx] |= 1 << bit_idx;
        }
        *bit_pos += 1;
    }
}

/// Inverse of [`write_bits`]: reads `num_bits` bits (`<= 128`) starting at
/// `*bit_pos` and returns them as the low bits of a `u128`. Advances
/// `*bit_pos` by `num_bits`. `num_bits == 0` reads nothing and returns `0`
/// — no special-casing needed, the loop simply does not execute.
fn read_bits(buf: &[u8], bit_pos: &mut usize, num_bits: u32) -> u128 {
    let mut value: u128 = 0;
    for i in 0..num_bits {
        let byte_idx = *bit_pos / 8;
        let bit_idx = *bit_pos % 8;
        if buf[byte_idx] & (1 << bit_idx) != 0 {
            value |= 1u128 << i;
        }
        *bit_pos += 1;
    }
    value
}

impl ValueCodec {
    /// Total packed width in bits: `key_tag_bits + balance_bits +
    /// checksum_bits`. This is the scalar `Geometry::for_accounts` sizes
    /// the whole deployment from (see `crate::geometry`).
    pub fn value_bits(&self) -> u32 {
        self.key_tag_bits + self.balance_bits + self.checksum_bits
    }

    fn check_widths(&self) -> Result<(), ValueError> {
        if self.key_tag_bits > 64 || self.balance_bits > 128 || self.checksum_bits > 64 {
            return Err(ValueError::InvalidWidth);
        }
        Ok(())
    }

    fn fits_balance_bits(&self, balance: Balance) -> bool {
        if self.balance_bits >= 128 {
            true
        } else {
            balance < (1u128 << self.balance_bits)
        }
    }

    /// FNV-1a-64 over the balance's low `ceil(balance_bits / 8)` LE bytes,
    /// truncated to `checksum_bits`. `0` if `checksum_bits == 0`.
    /// Independent of `key_tag` / `addr` by construction — it is a
    /// checksum of the *balance* only (see the struct-level docs).
    fn checksum(&self, balance: Balance) -> u64 {
        if self.checksum_bits == 0 {
            return 0;
        }
        let nbytes = (self.balance_bits as usize).div_ceil(8);
        let bytes = balance.to_le_bytes();
        let hash = fnv1a64(&bytes[..nbytes]);
        if self.checksum_bits >= 64 {
            hash
        } else {
            hash & ((1u64 << self.checksum_bits) - 1)
        }
    }

    /// `key_tag = H₂(address)`: the value-side half of the 64-bit-effective
    /// fingerprint (ADR-0009, see [`KEY_TAG_SEED`]'s docs for why this is a
    /// *different* hash from the store's own positioning fingerprint).
    /// Masked to the low `key_tag_bits` bits: `0` if `key_tag_bits == 0`
    /// (the check is disabled), the full 64-bit hash if `key_tag_bits >=
    /// 64`.
    pub fn key_tag(&self, addr: &AddressHash) -> u64 {
        if self.key_tag_bits == 0 {
            return 0;
        }
        let full = xxh3_64_with_seed(addr, KEY_TAG_SEED);
        if self.key_tag_bits >= 64 {
            full
        } else {
            full & ((1u64 << self.key_tag_bits) - 1)
        }
    }

    /// Reads the low `key_tag_bits` bits of `value_bytes` — the `key_tag`
    /// stored there, with no comparison and no length validation. Pure:
    /// `key_tag_bits == 0` yields `0` for free, since [`read_bits`]'s loop
    /// simply does not execute for zero bits — no explicit branch needed.
    ///
    /// # Constraints
    ///
    /// `value_bytes` must have at least `ceil(key_tag_bits / 8)` bytes —
    /// callers pass slot value bytes already sized from this same codec's
    /// `value_bits` (which includes `key_tag_bits` as its low bits), so
    /// this is a precondition, not a runtime check (mirrors
    /// [`write_bits`]/[`read_bits`]'s own preconditions).
    pub fn extract_key_tag(&self, value_bytes: &[u8]) -> u64 {
        let mut pos = 0usize;
        read_bits(value_bytes, &mut pos, self.key_tag_bits) as u64
    }

    /// Encodes `key_tag(addr) ‖ balance ‖ checksum(balance)` into
    /// `ceil(value_bits / 8)` bytes, little-endian, `key_tag` in the low
    /// bits.
    ///
    /// # Errors
    ///
    /// [`ValueError::InvalidWidth`] if this codec's widths are
    /// unrepresentable (see the struct-level constraints note).
    /// [`ValueError::BalanceOverflow`] if `balance >= 2^balance_bits` —
    /// **the balance is never truncated to fit.**
    pub fn encode(&self, addr: &AddressHash, balance: Balance) -> Result<Vec<u8>, ValueError> {
        self.check_widths()?;
        if !self.fits_balance_bits(balance) {
            return Err(ValueError::BalanceOverflow);
        }

        let value_bits = self.value_bits();
        let num_bytes = (value_bits as usize).div_ceil(8);
        let mut buf = vec![0u8; num_bytes];
        let mut pos = 0usize;
        write_bits(&mut buf, &mut pos, u128::from(self.key_tag(addr)), self.key_tag_bits);
        write_bits(&mut buf, &mut pos, balance, self.balance_bits);
        let checksum = self.checksum(balance);
        write_bits(&mut buf, &mut pos, u128::from(checksum), self.checksum_bits);
        debug_assert_eq!(pos, value_bits as usize);
        Ok(buf)
    }

    /// Decodes `value_bytes` against the claimed `addr` and reports which
    /// of the three [`Lookup`] outcomes applies. **Total — never panics in
    /// release, never returns a `Result`:** every input, however malformed,
    /// maps to one of `Found` / `NotFound` / `DecodeFailed`. This is what
    /// lets `risepir-client`'s per-slot scan call it uniformly without a
    /// fallible branch for "this candidate slot's fingerprint matched but
    /// it turned out not to be our key" — that case is exactly
    /// [`Lookup::NotFound`], not an error.
    ///
    /// # Scan order
    ///
    /// 1. This codec's own widths must be representable — if not, that is
    ///    a **construction bug** (the same condition [`Self::encode`]
    ///    reports as [`ValueError::InvalidWidth`]), so a debug build fires
    ///    a `debug_assert!` in addition to returning
    ///    [`Lookup::DecodeFailed`]; a release build silently returns
    ///    `DecodeFailed` rather than panicking on live traffic.
    /// 2. `value_bytes.len()` must equal `ceil(value_bits / 8)` — a
    ///    mismatch (wrong geometry, corrupt slice) is
    ///    [`Lookup::DecodeFailed`].
    /// 3. The stored `key_tag` must equal `self.key_tag(addr)` — a
    ///    mismatch means this slot is not `addr`'s entry, so
    ///    [`Lookup::NotFound`] (*not* an error: the caller is expected to
    ///    keep scanning other candidate slots).
    /// 4. The stored checksum must equal `checksum(balance)` — a mismatch
    ///    means the balance decoded corrupt, so [`Lookup::DecodeFailed`]
    ///    (never a number).
    /// 5. Otherwise, [`Lookup::Found`]`(balance)`.
    pub fn decode(&self, addr: &AddressHash, value_bytes: &[u8]) -> Lookup {
        if self.check_widths().is_err() {
            debug_assert!(
                false,
                "ValueCodec::decode: key_tag_bits/balance_bits/checksum_bits unrepresentable \
                 (key_tag_bits <= 64, balance_bits <= 128, checksum_bits <= 64) — this is a \
                 construction bug, not a runtime data problem"
            );
            return Lookup::DecodeFailed;
        }

        let value_bits = self.value_bits();
        let expected = (value_bits as usize).div_ceil(8);
        if value_bytes.len() != expected {
            return Lookup::DecodeFailed;
        }

        let mut pos = 0usize;
        let stored_key_tag = read_bits(value_bytes, &mut pos, self.key_tag_bits) as u64;
        let balance = read_bits(value_bytes, &mut pos, self.balance_bits);
        let stored_checksum = read_bits(value_bytes, &mut pos, self.checksum_bits) as u64;
        debug_assert_eq!(pos, value_bits as usize);

        if stored_key_tag != self.key_tag(addr) {
            return Lookup::NotFound;
        }
        if stored_checksum != self.checksum(balance) {
            return Lookup::DecodeFailed;
        }
        Lookup::Found(balance)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic distinct address for index `i` — mirrors the `addr()`
    /// helper `risepir-server`/`risepir-client`'s own test modules use.
    fn test_addr(i: u64) -> AddressHash {
        let mut a = [0u8; 32];
        a[..8].copy_from_slice(&i.to_le_bytes());
        a
    }

    #[test]
    fn round_trip_basic() {
        let codec = ValueCodec {
            key_tag_bits: 32,
            balance_bits: 96,
            checksum_bits: 16,
        };
        let addr = test_addr(1);
        let balance: Balance = 123_456_789_012_345_678_901_234;
        let encoded = codec.encode(&addr, balance).unwrap();
        assert_eq!(encoded.len(), (32 + 96 + 16) / 8);
        assert_eq!(codec.decode(&addr, &encoded), Lookup::Found(balance));
    }

    #[test]
    fn round_trip_zero_checksum_bits_disables_check() {
        let codec = ValueCodec {
            key_tag_bits: 32,
            balance_bits: 96,
            checksum_bits: 0,
        };
        let addr = test_addr(2);
        let balance: Balance = 42;
        let encoded = codec.encode(&addr, balance).unwrap();
        assert_eq!(encoded.len(), (32 + 96) / 8);
        assert_eq!(codec.decode(&addr, &encoded), Lookup::Found(balance));
    }

    #[test]
    fn round_trip_zero_key_tag_bits_disables_check() {
        let codec = ValueCodec {
            key_tag_bits: 0,
            balance_bits: 96,
            checksum_bits: 16,
        };
        let addr = test_addr(3);
        let other = test_addr(4);
        let balance: Balance = 7;
        let encoded = codec.encode(&addr, balance).unwrap();
        // With key_tag_bits == 0 the tag check is disabled, so *any*
        // address decodes the same slot successfully — this is the
        // documented, deliberate cost of disabling the tag, not a bug.
        assert_eq!(codec.decode(&addr, &encoded), Lookup::Found(balance));
        assert_eq!(codec.decode(&other, &encoded), Lookup::Found(balance));
    }

    #[test]
    fn round_trip_non_byte_aligned_widths() {
        // 5 + 96 + 13 = 114 bits -> 15 bytes, with 6 padding bits in the
        // last byte.
        let codec = ValueCodec {
            key_tag_bits: 5,
            balance_bits: 96,
            checksum_bits: 13,
        };
        let addr = test_addr(5);
        let balance: Balance = u128::from(u64::MAX) + 7;
        let encoded = codec.encode(&addr, balance).unwrap();
        assert_eq!(encoded.len(), 15);
        assert_eq!(codec.decode(&addr, &encoded), Lookup::Found(balance));
    }

    #[test]
    fn round_trip_full_width_balance() {
        let codec = ValueCodec {
            key_tag_bits: 32,
            balance_bits: 128,
            checksum_bits: 32,
        };
        let addr = test_addr(6);
        for balance in [0u128, 1, Balance::MAX] {
            let encoded = codec.encode(&addr, balance).unwrap();
            assert_eq!(codec.decode(&addr, &encoded), Lookup::Found(balance));
        }
    }

    /// NEW (ADR-0009): decoding a genuine encoding under the *wrong*
    /// address must report `NotFound` — the `key_tag` mismatch — not
    /// `Found` with some other balance and not `DecodeFailed`.
    #[test]
    fn decode_wrong_addr_is_not_found() {
        let codec = ValueCodec {
            key_tag_bits: 32,
            balance_bits: 96,
            checksum_bits: 16,
        };
        let addr = test_addr(7);
        let other = test_addr(8);
        let balance: Balance = 555_000_000_000_000_000_000u128;
        let encoded = codec.encode(&addr, balance).unwrap();
        assert_eq!(codec.decode(&addr, &encoded), Lookup::Found(balance));
        assert_eq!(codec.decode(&other, &encoded), Lookup::NotFound);
    }

    #[test]
    fn encode_rejects_overflowing_balance_never_truncates() {
        let codec = ValueCodec {
            key_tag_bits: 8,
            balance_bits: 8,
            checksum_bits: 8,
        };
        let addr = test_addr(9);
        assert_eq!(codec.encode(&addr, 256), Err(ValueError::BalanceOverflow));
        assert_eq!(codec.encode(&addr, Balance::MAX), Err(ValueError::BalanceOverflow));
        // The boundary itself must still succeed, proving this isn't an
        // off-by-one rejecting valid balances too.
        assert!(codec.encode(&addr, 255).is_ok());
    }

    #[test]
    fn decode_wrong_length_is_decode_failed() {
        let codec = ValueCodec {
            key_tag_bits: 32,
            balance_bits: 96,
            checksum_bits: 16,
        };
        let addr = test_addr(10);
        assert_eq!(codec.decode(&addr, &[0u8; 13]), Lookup::DecodeFailed);
    }

    #[test]
    fn invalid_widths_rejected() {
        let addr = test_addr(11);
        let codec = ValueCodec {
            key_tag_bits: 65,
            balance_bits: 32,
            checksum_bits: 0,
        };
        assert_eq!(codec.encode(&addr, 0), Err(ValueError::InvalidWidth));

        let codec = ValueCodec {
            key_tag_bits: 0,
            balance_bits: 129,
            checksum_bits: 0,
        };
        assert_eq!(codec.encode(&addr, 0), Err(ValueError::InvalidWidth));

        let codec = ValueCodec {
            key_tag_bits: 0,
            balance_bits: 8,
            checksum_bits: 65,
        };
        assert_eq!(codec.encode(&addr, 0), Err(ValueError::InvalidWidth));
    }

    /// `decode`'s width check is total in release (returns
    /// `DecodeFailed`) but fires a loud `debug_assert!` in debug/test
    /// builds, since reaching it at all is a construction bug, not live
    /// data. Pins that the assert message identifies it as such.
    #[test]
    #[should_panic(expected = "construction bug")]
    fn decode_invalid_widths_panics_in_debug() {
        let addr = test_addr(12);
        let codec = ValueCodec {
            key_tag_bits: 65,
            balance_bits: 32,
            checksum_bits: 0,
        };
        let _ = codec.decode(&addr, &[0u8; 100]);
    }

    /// Flipping one bit inside the balance region must be caught by the
    /// checksum. This is the corruption the whole module exists to turn
    /// into a loud [`Lookup::DecodeFailed`] instead of a plausible wrong
    /// balance.
    #[test]
    fn corrupt_one_bit_in_balance_is_detected() {
        let codec = ValueCodec {
            key_tag_bits: 32,
            balance_bits: 96,
            checksum_bits: 16,
        };
        let addr = test_addr(13);
        let balance: Balance = 0xdead_beef_1234_5678_9abc_u128;
        let mut encoded = codec.encode(&addr, balance).unwrap();
        // Flip the low bit of the balance's first byte — byte index 4,
        // since the 32-bit (4-byte) key_tag occupies bytes 0..4 in this
        // layout (key_tag is in the *low* bits; see the struct docs).
        encoded[4] ^= 0x01;
        assert_eq!(codec.decode(&addr, &encoded), Lookup::DecodeFailed);
    }

    /// Sweep every bit position in the whole encoding (key_tag, balance,
    /// and checksum regions alike) to make sure no single-bit flip
    /// anywhere silently produces a different-but-accepted balance. Every
    /// outcome must be `Found(balance)` (the corruption happened to be
    /// harmless, e.g. padding bits), `NotFound` (the flip landed in the
    /// key_tag region and broke the match), or `DecodeFailed` (the flip
    /// broke the checksum) — never `Found` with a *different* balance.
    #[test]
    fn corrupt_any_single_bit_is_detected() {
        let codec = ValueCodec {
            key_tag_bits: 32,
            balance_bits: 96,
            checksum_bits: 16,
        };
        let addr = test_addr(14);
        let balance: Balance = 999_999_999_999_999_999_999;
        let encoded = codec.encode(&addr, balance).unwrap();
        for byte_idx in 0..encoded.len() {
            for bit_idx in 0..8u8 {
                let mut corrupted = encoded.clone();
                corrupted[byte_idx] ^= 1 << bit_idx;
                match codec.decode(&addr, &corrupted) {
                    Lookup::DecodeFailed | Lookup::NotFound => {}
                    Lookup::Found(other) => assert_eq!(
                        other, balance,
                        "byte {byte_idx} bit {bit_idx}: corruption accepted with a *different* balance and no error"
                    ),
                }
            }
        }
    }

    proptest::proptest! {
        #[test]
        fn round_trip_prop(
            key_tag_bits in 0u32..=64,
            balance_bits in 1u32..=128,
            checksum_bits in 0u32..=64,
            raw in proptest::array::uniform16(proptest::num::u8::ANY),
            addr_bytes in proptest::array::uniform32(proptest::num::u8::ANY),
        ) {
            let codec = ValueCodec { key_tag_bits, balance_bits, checksum_bits };
            let addr: AddressHash = addr_bytes;
            let full = u128::from_le_bytes(raw);
            let balance = if balance_bits >= 128 { full } else { full % (1u128 << balance_bits) };
            let encoded = codec.encode(&addr, balance).unwrap();
            proptest::prop_assert_eq!(
                encoded.len(),
                ((key_tag_bits + balance_bits + checksum_bits) as usize).div_ceil(8)
            );
            proptest::prop_assert_eq!(codec.decode(&addr, &encoded), Lookup::Found(balance));
        }

        #[test]
        fn overflow_prop(
            key_tag_bits in 0u32..=64,
            balance_bits in 1u32..128,
            raw in proptest::array::uniform16(proptest::num::u8::ANY),
            addr_bytes in proptest::array::uniform32(proptest::num::u8::ANY),
        ) {
            let codec = ValueCodec { key_tag_bits, balance_bits, checksum_bits: 8 };
            let addr: AddressHash = addr_bytes;
            let full = u128::from_le_bytes(raw);
            // `extra < 2^(128 - balance_bits)`, so `extra + 2^balance_bits`
            // never nears `u128::MAX`; plain `+` (which panics on overflow
            // in test builds) is deliberate here rather than `saturating_add`,
            // so a wrong bound in this test fails loudly instead of being
            // silently masked.
            let extra = full % (1u128 << (128 - balance_bits));
            let over = extra + (1u128 << balance_bits);
            proptest::prop_assert!(over >= (1u128 << balance_bits));
            proptest::prop_assert_eq!(codec.encode(&addr, over), Err(ValueError::BalanceOverflow));
        }
    }
}
