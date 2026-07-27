//! `RPJL1` delta-journal reader under adversarial bytes (ADR-0026). The
//! journal is parsed with the same hostile-input posture as the state
//! file (`state_load.rs`): a corrupt, truncated, or hostile file must
//! never panic or OOM. It must *stop* — never error the whole read — at
//! the first invalid record and report why
//! (`risepir_rpc::journal::ScanStop`), streaming one record at a time
//! rather than materializing the whole file.

#![no_main]

use libfuzzer_sys::fuzz_target;
use risepir_rpc::journal::JournalReader;

fuzz_target!(|data: &[u8]| {
    // Fixed geometry constants, mirroring `state_load.rs`'s style: the
    // journal format does not carry `plaintext_bits`/arity itself (the
    // caller always supplies them from the base's own setup params), so
    // any fixed valid pair exercises the decoder's hostile-input handling
    // just as thoroughly as a real deployment's would.
    const PLAINTEXT_BITS: u32 = 8;
    const ARITY: u32 = 3;

    let Ok((_header, mut reader)) = JournalReader::open(data, data.len() as u64, PLAINTEXT_BITS, ARITY) else {
        return;
    };
    // Drain the whole scan without ever materializing more than one
    // record at a time, until it stops (clean EOF, or the first invalid
    // record — never a panic or an unbounded allocation either way).
    while reader.next().is_some() {}
});
