//! `RPST1`/`RPST2` state-file loader under adversarial bytes. State files
//! are operator-trusted in the threat model, but the loader's contract is
//! the same as every decoder's: a corrupt, truncated, or hostile file is
//! a clean [`risepir_rpc::state::StateError`] — never a panic and never
//! an allocation sized from an unvalidated header (`setup_len` is bounded
//! by the input's own length before any `Vec` is sized from it).

#![no_main]

use libfuzzer_sys::fuzz_target;
use risepir_proto::ValueCodec;

fuzz_target!(|data: &[u8]| {
    let codec = ValueCodec {
        key_tag_bits: 32,
        balance_bits: 96,
        checksum_bits: 16,
    };
    let config = ikpir_common::SimpleConfig::with_lwe_dim(256);
    let _ = risepir_rpc::state::load_bytes(data, config, &codec);
});
