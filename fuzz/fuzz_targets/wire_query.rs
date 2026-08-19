//! `POST /answer` request decoder under adversarial bytes — the single
//! most attacker-facing decoder in the system (the body is untrusted,
//! multi-hundred-KB input, and the decoded queries feed straight into the
//! server's matvec, which indexes on the *exact*-length assumption).
//!
//! The first two input bytes steer the deployment geometry the decoder
//! is checking against (per-segment expected lengths in 1..=64), so the
//! fuzzer explores the interaction of wire bytes × geometry, not one
//! hard-coded shape.

#![no_main]

use libfuzzer_sys::fuzz_target;
use risepir_http::wire::{decode_query_bundle, encode_query_bundle};
use segmented_cuckoo::{CuckooParams, SchemeKind};

fuzz_target!(|data: &[u8]| {
    let [l0, l1, rest @ ..] = data else { return };
    let expected_len_per_seg = [u32::from(l0 % 64) + 1, u32::from(l1 % 64) + 1];
    let params = CuckooParams {
        // arity 2 (ADR-0034) — matches this repo's deployed geometry.
        scheme_kind: SchemeKind::Segmented2ary,
        num_buckets: 64,
        bucket_size: 4,
        fingerprint_bits: 32,
        value_bits: 96,
        plaintext_bits: 8,
    };

    if let Ok(queries) = decode_query_bundle(rest, &params, &expected_len_per_seg) {
        let canonical = encode_query_bundle(&queries);
        let again = decode_query_bundle(&canonical, &params, &expected_len_per_seg)
            .expect("canonical encoding must decode");
        assert_eq!(encode_query_bundle(&again), canonical, "decode/encode must be idempotent");
    }
});
