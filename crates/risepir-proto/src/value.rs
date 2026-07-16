//! The value encoding: `balance ‖ checksum`, packed into `value_bits` bits.
//!
//! # Purpose
//!
//! This is the "never return a wrong answer" surface named in
//! `docs/plan.md`'s enforcement table: LWE decode noise can corrupt a
//! value cell while leaving the fingerprint cells intact, in which case an
//! unprotected scheme would return a *plausible, silently wrong* balance.
//! [`ValueCodec`] carries a checksum inside the value bits specifically so
//! that failure mode turns into a loud [`ValueError::ChecksumMismatch`]
//! instead.

/// A balance in wei — re-exported at crate root as `risepir_proto::Balance`;
/// imported here to give [`ValueCodec::encode`] / [`ValueCodec::decode`]
/// concrete signatures.
pub type Balance = crate::types::Balance;

/// Packs/unpacks `balance ‖ checksum` as a little-endian bitstring of
/// `balance_bits + checksum_bits` bits, stored in `ceil((balance_bits +
/// checksum_bits) / 8)` bytes.
///
/// # Checksum function
///
/// FNV-1a-64 (offset basis `0xcbf29ce484222325`, prime `0x100000001b3`) over
/// `balance`'s little-endian bytes, truncated to the low `balance_bits` bits
/// (i.e. hashed over exactly `ceil(balance_bits / 8)` bytes — no more, no
/// fewer), then truncated to the low `checksum_bits` bits of the resulting
/// 64-bit hash. Deterministic; no keying, no randomness.
///
/// # Constraints
///
/// `balance_bits <= 128` (must fit [`Balance`]) and `checksum_bits <= 64`
/// (must fit the FNV-1a-64 output); `encode` / `decode` return
/// [`ValueError::InvalidWidth`] otherwise. `checksum_bits == 0` is
/// supported and disables the checksum (every value decodes to a `0`
/// checksum and the comparison always passes) — this exists so the cost of
/// carrying a checksum can be measured against not carrying one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ValueCodec {
    /// Width of the balance field in bits.
    pub balance_bits: u32,
    /// Width of the checksum field in bits. `0` disables the checksum.
    pub checksum_bits: u32,
}

/// Errors from [`ValueCodec::encode`] / [`ValueCodec::decode`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ValueError {
    /// `encode` was asked to pack a `balance >= 2^balance_bits`. The
    /// balance is never truncated to fit — this is a hard failure by
    /// design (`docs/plan.md` ADR-0009).
    BalanceOverflow,
    /// `decode` recomputed the checksum over the decoded balance bytes and
    /// it did not match the stored checksum. The balance is **not**
    /// returned in this case — a mismatch means "decode failed", not "here
    /// is a possibly-wrong number".
    ChecksumMismatch,
    /// `balance_bits > 128` or `checksum_bits > 64`, which this codec
    /// cannot represent (see the struct-level constraints note).
    InvalidWidth,
    /// `decode` was given a byte slice of the wrong length for this
    /// codec's `balance_bits + checksum_bits`.
    InvalidLength {
        /// Expected length: `ceil((balance_bits + checksum_bits) / 8)`.
        expected: usize,
        /// Actual length of the slice passed to `decode`.
        found: usize,
    },
}

impl std::fmt::Display for ValueError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BalanceOverflow => write!(f, "balance does not fit in balance_bits"),
            Self::ChecksumMismatch => write!(f, "checksum mismatch: decoded value is corrupt"),
            Self::InvalidWidth => {
                write!(f, "balance_bits must be <= 128 and checksum_bits must be <= 64")
            }
            Self::InvalidLength { expected, found } => {
                write!(f, "expected {expected} bytes, found {found}")
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
/// `*bit_pos` by `num_bits`.
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
    /// Total packed width in bits: `balance_bits + checksum_bits`.
    fn value_bits(&self) -> u32 {
        self.balance_bits + self.checksum_bits
    }

    fn check_widths(&self) -> Result<(), ValueError> {
        if self.balance_bits > 128 || self.checksum_bits > 64 {
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

    /// Encodes `balance ‖ checksum(balance)` into `ceil(value_bits / 8)`
    /// bytes, little-endian.
    ///
    /// # Errors
    ///
    /// [`ValueError::InvalidWidth`] if this codec's widths are
    /// unrepresentable (see the struct-level constraints note).
    /// [`ValueError::BalanceOverflow`] if `balance >= 2^balance_bits` —
    /// **the balance is never truncated to fit.**
    pub fn encode(&self, balance: Balance) -> Result<Vec<u8>, ValueError> {
        self.check_widths()?;
        if !self.fits_balance_bits(balance) {
            return Err(ValueError::BalanceOverflow);
        }

        let value_bits = self.value_bits();
        let num_bytes = (value_bits as usize).div_ceil(8);
        let mut buf = vec![0u8; num_bytes];
        let mut pos = 0usize;
        write_bits(&mut buf, &mut pos, balance, self.balance_bits);
        let checksum = self.checksum(balance);
        write_bits(&mut buf, &mut pos, u128::from(checksum), self.checksum_bits);
        debug_assert_eq!(pos, value_bits as usize);
        Ok(buf)
    }

    /// Decodes `bytes` and verifies the checksum.
    ///
    /// # Errors
    ///
    /// [`ValueError::InvalidWidth`] if this codec's widths are
    /// unrepresentable. [`ValueError::InvalidLength`] if `bytes.len()` is
    /// not exactly `ceil(value_bits / 8)`. [`ValueError::ChecksumMismatch`]
    /// if the recomputed checksum disagrees with the stored one — **no
    /// balance is returned in that case**, by design: a mismatch means the
    /// decode is untrustworthy, not merely suspicious.
    pub fn decode(&self, bytes: &[u8]) -> Result<Balance, ValueError> {
        self.check_widths()?;
        let value_bits = self.value_bits();
        let expected = (value_bits as usize).div_ceil(8);
        if bytes.len() != expected {
            return Err(ValueError::InvalidLength {
                expected,
                found: bytes.len(),
            });
        }

        let mut pos = 0usize;
        let balance = read_bits(bytes, &mut pos, self.balance_bits);
        let stored_checksum = read_bits(bytes, &mut pos, self.checksum_bits) as u64;
        debug_assert_eq!(pos, value_bits as usize);

        if stored_checksum != self.checksum(balance) {
            return Err(ValueError::ChecksumMismatch);
        }
        Ok(balance)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_basic() {
        let codec = ValueCodec {
            balance_bits: 96,
            checksum_bits: 16,
        };
        let balance: Balance = 123_456_789_012_345_678_901_234;
        let encoded = codec.encode(balance).unwrap();
        assert_eq!(encoded.len(), (96 + 16) / 8);
        assert_eq!(codec.decode(&encoded).unwrap(), balance);
    }

    #[test]
    fn round_trip_zero_checksum_bits_disables_check() {
        let codec = ValueCodec {
            balance_bits: 96,
            checksum_bits: 0,
        };
        let balance: Balance = 42;
        let encoded = codec.encode(balance).unwrap();
        assert_eq!(encoded.len(), 96 / 8);
        assert_eq!(codec.decode(&encoded).unwrap(), balance);
    }

    #[test]
    fn round_trip_non_byte_aligned_widths() {
        // 96 + 13 = 109 bits -> 14 bytes, with 3 padding bits in the last byte.
        let codec = ValueCodec {
            balance_bits: 96,
            checksum_bits: 13,
        };
        let balance: Balance = u128::from(u64::MAX) + 7;
        let encoded = codec.encode(balance).unwrap();
        assert_eq!(encoded.len(), 14);
        assert_eq!(codec.decode(&encoded).unwrap(), balance);
    }

    #[test]
    fn round_trip_full_width_balance() {
        let codec = ValueCodec {
            balance_bits: 128,
            checksum_bits: 32,
        };
        for balance in [0u128, 1, Balance::MAX] {
            let encoded = codec.encode(balance).unwrap();
            assert_eq!(codec.decode(&encoded).unwrap(), balance);
        }
    }

    #[test]
    fn encode_rejects_overflowing_balance_never_truncates() {
        let codec = ValueCodec {
            balance_bits: 8,
            checksum_bits: 8,
        };
        assert_eq!(codec.encode(256), Err(ValueError::BalanceOverflow));
        assert_eq!(codec.encode(Balance::MAX), Err(ValueError::BalanceOverflow));
        // The boundary itself must still succeed.
        assert!(codec.encode(255).is_ok());
    }

    #[test]
    fn decode_rejects_wrong_length() {
        let codec = ValueCodec {
            balance_bits: 96,
            checksum_bits: 16,
        };
        let err = codec.decode(&[0u8; 13]).unwrap_err();
        assert_eq!(
            err,
            ValueError::InvalidLength {
                expected: 14,
                found: 13
            }
        );
    }

    #[test]
    fn invalid_widths_rejected() {
        let codec = ValueCodec {
            balance_bits: 129,
            checksum_bits: 0,
        };
        assert_eq!(codec.encode(0), Err(ValueError::InvalidWidth));
        let codec = ValueCodec {
            balance_bits: 8,
            checksum_bits: 65,
        };
        assert_eq!(codec.encode(0), Err(ValueError::InvalidWidth));
    }

    /// Flipping one bit inside the balance region must be caught by the
    /// checksum. This is the corruption the whole module exists to turn
    /// into a loud error instead of a plausible wrong balance.
    #[test]
    fn corrupt_one_bit_in_balance_is_detected() {
        let codec = ValueCodec {
            balance_bits: 96,
            checksum_bits: 16,
        };
        let balance: Balance = 0xdead_beef_1234_5678_9abc_u128;
        let mut encoded = codec.encode(balance).unwrap();
        // Flip the low bit of the first balance byte.
        encoded[0] ^= 0x01;
        assert_eq!(codec.decode(&encoded), Err(ValueError::ChecksumMismatch));
    }

    /// Same, but sweep every bit position in the whole encoding (balance
    /// and checksum region alike) to make sure no single-bit flip anywhere
    /// silently produces a different-but-accepted balance.
    #[test]
    fn corrupt_any_single_bit_is_detected() {
        let codec = ValueCodec {
            balance_bits: 96,
            checksum_bits: 16,
        };
        let balance: Balance = 999_999_999_999_999_999_999;
        let encoded = codec.encode(balance).unwrap();
        for byte_idx in 0..encoded.len() {
            for bit_idx in 0..8u8 {
                let mut corrupted = encoded.clone();
                corrupted[byte_idx] ^= 1 << bit_idx;
                match codec.decode(&corrupted) {
                    Err(ValueError::ChecksumMismatch) => {}
                    Ok(other) => assert_eq!(
                        other, balance,
                        "byte {byte_idx} bit {bit_idx}: corruption accepted with a *different* balance and no error"
                    ),
                    Err(other) => panic!("unexpected error at byte {byte_idx} bit {bit_idx}: {other:?}"),
                }
            }
        }
    }

    proptest::proptest! {
        #[test]
        fn round_trip_prop(
            balance_bits in 1u32..=128,
            checksum_bits in 0u32..=64,
            raw in proptest::array::uniform16(proptest::num::u8::ANY),
        ) {
            let codec = ValueCodec { balance_bits, checksum_bits };
            let full = u128::from_le_bytes(raw);
            let balance = if balance_bits >= 128 { full } else { full % (1u128 << balance_bits) };
            let encoded = codec.encode(balance).unwrap();
            proptest::prop_assert_eq!(encoded.len(), ((balance_bits + checksum_bits) as usize).div_ceil(8));
            proptest::prop_assert_eq!(codec.decode(&encoded).unwrap(), balance);
        }

        #[test]
        fn overflow_prop(
            balance_bits in 1u32..128,
            raw in proptest::array::uniform16(proptest::num::u8::ANY),
        ) {
            let codec = ValueCodec { balance_bits, checksum_bits: 8 };
            let full = u128::from_le_bytes(raw);
            // `extra < 2^(128 - balance_bits)`, so `extra + 2^balance_bits`
            // never nears `u128::MAX`; plain `+` (which panics on overflow
            // in test builds) is deliberate here rather than `saturating_add`,
            // so a wrong bound in this test fails loudly instead of being
            // silently masked.
            let extra = full % (1u128 << (128 - balance_bits));
            let over = extra + (1u128 << balance_bits);
            proptest::prop_assert!(over >= (1u128 << balance_bits));
            proptest::prop_assert_eq!(codec.encode(over), Err(ValueError::BalanceOverflow));
        }
    }
}
