//! `GET /setup` response decoder under adversarial bytes.
//!
//! Oracle, both halves of the repo's input rule:
//! 1. no panic, no unbounded allocation, on any input (the decoder's
//!    documented contract);
//! 2. anything that *does* decode re-encodes canonically: decode ∘ encode
//!    is the identity on canonical bytes. (Raw input byte-identity is
//!    deliberately not asserted — `read_uvarint` accepts non-minimal
//!    varints, so adversarial input is not always canonical.)

#![no_main]

use libfuzzer_sys::fuzz_target;
use risepir_http::wire::{decode_setup, encode_setup};

fuzz_target!(|data: &[u8]| {
    if let Ok(bundle) = decode_setup(data) {
        let canonical = encode_setup(&bundle);
        let again = decode_setup(&canonical).expect("canonical encoding must decode");
        assert_eq!(encode_setup(&again), canonical, "decode/encode must be idempotent");
    }
});
