//! `keccak256` — the SCF key (`docs/plan.md` ADR-0008: "free (the SCF
//! hashes the key anyway), the client computes it itself, and it matches
//! reth's hashed state").

use crate::types::AddressHash;

/// `keccak256(address)`, Ethereum's actual Keccak (the pre-NIST padding —
/// delimiter `0x01`, *not* SHA3's `0x06`). `tiny_keccak`'s `keccak256`
/// free function is exactly this: verified against `cast keccak` for both
/// the empty input and `b"abc"` during development (this crate has no
/// other way to pull in a reference vector without a second crypto
/// dependency, and those two vectors are the standard ones every Keccak
/// implementation is checked against).
pub fn keccak256(addr20: &[u8; 20]) -> AddressHash {
    tiny_keccak::keccak256(addr20)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// `keccak256("")` — the single most load-bearing Ethereum constant to
    /// get right (it is, among other things, `EXTCODEHASH` of every
    /// non-contract account). The expected value below was cross-checked
    /// byte-for-byte against `cast keccak 0x` (Foundry) during development,
    /// not typed from memory.
    #[test]
    fn matches_known_empty_input_vector() {
        // Calls the underlying `tiny_keccak::keccak256` on the true empty
        // slice directly — the reference vector every Keccak implementation
        // publishes — rather than through this module's `&[u8; 20]`-typed
        // wrapper, which has no 0-length input to give it.
        let got = tiny_keccak::keccak256(&[]);
        assert_eq!(
            hex(&got),
            "c5d2460186f7233c927e7db2dcc703c0e500b653ca82273b7bfad8045d85a470",
            "keccak256(\"\") mismatch — this constant is EXTCODEHASH of every EOA"
        );
    }

    /// Distinct addresses must hash to distinct keys (collision would be
    /// catastrophic — sanity only, not a security proof).
    #[test]
    fn distinct_addresses_hash_distinctly() {
        let a = keccak256(&[0x11; 20]);
        let b = keccak256(&[0x22; 20]);
        assert_ne!(a, b);
    }

    /// `keccak256` of a 20-byte address is exactly the low-level function's
    /// output over those same bytes — pins that the `&[u8; 20] -> &[u8]`
    /// coercion this module relies on is not silently including a length
    /// prefix or padding.
    #[test]
    fn matches_direct_slice_hash() {
        let addr = [0x42u8; 20];
        assert_eq!(keccak256(&addr), tiny_keccak::keccak256(&addr[..]));
    }
}
