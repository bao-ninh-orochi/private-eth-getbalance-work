#![warn(missing_docs)]
//! The RisePIR private-`eth_getBalance` client, compiled for the browser
//! (ADR-0019) — a pure-compute wasm32 module. **It never touches the
//! network.** The host does every fetch and hands the bytes in; this
//! module hashes the address, builds the LWE query, rewinds the response
//! against the public delta stream, and scans. The address exists only
//! inside this module's linear memory, on the user's own machine.
//!
//! # Why this, rather than a backend that does the query for you
//!
//! A web front end that POSTs an address to a server and gets a balance
//! back is a fast balance API with extra steps: the server learns the
//! address, which is the one thing this project exists to prevent. Moving
//! query construction into the page is what keeps the deployed privacy
//! property the same one the protocol proves. What that does *not* cover
//! is stated plainly in the UI and in ADR-0019: the code delivering this
//! module is itself trusted, and the network layer still sees who is
//! asking, when, and from where.
//!
//! # The host ABI
//!
//! No `wasm-bindgen`: this is a plain `cdylib` with C-ABI exports, so the
//! artifact is exactly what `cargo build --target wasm32-unknown-unknown`
//! produced, with no code generator between the source and the audit.
//!
//! **The host never gives this module a pointer.** Both buffers belong to
//! Rust: the host asks for an input buffer of a size ([`abi::risepir_in_reserve`]),
//! writes into the region [`abi::risepir_in_ptr`] reports, and calls an
//! operation that reads it; results come back through [`abi::risepir_out_ptr`] /
//! [`abi::risepir_out_len`]. Nothing here dereferences a caller-supplied
//! address, which is why the only `unsafe` in the crate is the entropy
//! shim (see [`entropy`]).
//!
//! ```text
//! risepir_in_reserve(len)     grow the input buffer; call before every write
//! risepir_in_ptr()            where to write (RE-READ after every reserve:
//!                             both this pointer and memory.buffer move)
//! risepir_out_ptr()/_len()    the last result produced
//! risepir_err_ptr()/_len()    UTF-8 message for the last failure
//!
//! risepir_set_mode(len)       ingest GET /mode's body (exactly one byte)
//! risepir_init(len)           ingest GET /setup's body; builds the client
//! risepir_ingest(len)         ingest a GET /sync body   -> new pending head
//! risepir_query(len)          20-byte address in        -> OUT = /answer body
//! risepir_answer(len)         POST /answer's body in    -> block answered at
//! risepir_finish()            -> status; OUT = 16-byte LE u128 balance
//! ```
//!
//! Every entry point returns a negative value on failure and leaves a
//! legible reason in the error buffer; none of them panics on host input.
//! The status codes [`abi::risepir_finish`] returns are deliberately four, not
//! two — see [`session::Outcome`]: "absent" means `0x0` only for a
//! complete set, and the browser is not allowed to decide which it is
//! talking to.

pub mod abi;
pub mod entropy;
pub mod session;

pub use abi::{
    STATUS_DECODE_FAILED, STATUS_ERROR, STATUS_FOUND, STATUS_UNTRACKED, STATUS_ZERO,
};

