//! `BlockDelta` decoder under adversarial bytes: `GET /delta/{block}` and
//! `GET /sync` responses are fetched by every client from a server it does
//! not have to trust (threat model §4.2), and the delta stream is the
//! payload a hostile CDN or on-path attacker would tamper with.
//!
//! The first two input bytes steer `plaintext_bits` (1..=31, the range
//! `Geometry` can produce) and arity (2..=4, the real `SchemeKind`
//! arities), so the `|Δ| < p` wire bound is exercised at every plaintext
//! width, not one.

#![no_main]

use libfuzzer_sys::fuzz_target;
use risepir_proto::codec::{decode_block_delta, encode_block_delta};

fuzz_target!(|data: &[u8]| {
    let [b0, b1, rest @ ..] = data else { return };
    let plaintext_bits = u32::from(b0 % 31) + 1;
    let arity = u32::from(b1 % 3) + 2;

    if let Ok(delta) = decode_block_delta(rest, plaintext_bits, arity) {
        let canonical = encode_block_delta(&delta, plaintext_bits);
        let again = decode_block_delta(&canonical, plaintext_bits, arity)
            .expect("canonical encoding must decode");
        assert_eq!(
            encode_block_delta(&again, plaintext_bits),
            canonical,
            "decode/encode must be idempotent"
        );
    }
});
