//! `POST /answer` *response* decoder under adversarial bytes — this is the
//! client-side decoder, i.e. what a malicious or on-path *server* gets to
//! feed (threat model §4.2: tampered responses must fail cleanly, never
//! panic the client). Geometry steering as in `wire_query`.

#![no_main]

use libfuzzer_sys::fuzz_target;
use risepir_http::wire::{decode_response_bundle, encode_response_bundle};

fuzz_target!(|data: &[u8]| {
    let [l0, l1, l2, rest @ ..] = data else { return };
    let expected_len_per_seg =
        [u32::from(l0 % 64) + 1, u32::from(l1 % 64) + 1, u32::from(l2 % 64) + 1];

    if let Ok((responses, head)) = decode_response_bundle(rest, &expected_len_per_seg, 3) {
        let canonical = encode_response_bundle(&responses, head);
        let (again, head_again) = decode_response_bundle(&canonical, &expected_len_per_seg, 3)
            .expect("canonical encoding must decode");
        assert_eq!(head_again, head);
        assert_eq!(
            encode_response_bundle(&again, head_again),
            canonical,
            "decode/encode must be idempotent"
        );
    }
});
