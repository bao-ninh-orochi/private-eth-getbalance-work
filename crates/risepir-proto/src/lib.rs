#![forbid(unsafe_code)]
#![warn(missing_docs)]
//! Pure foundation types for the private `eth_getBalance` service (see
//! `docs/plan.md`): the geometry calculator, the block-update/delta types,
//! the value codec, and the compact wire codec.
//!
//! # Scope
//!
//! This crate does **no I/O and no async** — it is types, math, and byte
//! (de)serialization only. `risepir-server` and `risepir-client` build the
//! actual networked service on top of it and on top of `ikpir-common`'s
//! public PIR primitives.
//!
//! # The invariant everything here serves
//!
//! Per `docs/plan.md`: *never return a wrong answer*. Concretely, in this
//! crate that means: [`value::ValueCodec::encode`] hard-fails rather than
//! truncating an overflowing balance; [`value::ValueCodec::decode`] never
//! returns a `Result` and, on live data, never panics — it reports a
//! checksum mismatch or a `key_tag` mismatch as
//! [`value::Lookup::DecodeFailed`] / [`value::Lookup::NotFound`] rather
//! than ever returning a corrupted or misattributed number (a debug build
//! additionally fires a loud assert if the codec itself was constructed
//! with unrepresentable widths, since that is a construction bug, not
//! live data); and [`codec::decode_block_delta`] validates every count
//! against the input length before allocating, and every delta against
//! the plaintext modulus, so a malformed or hostile byte stream produces a
//! clean [`codec::CodecError`] instead of a panic, an OOM, or — worst of
//! all — a plausible-looking wrong value.

pub mod codec;
pub mod geometry;
pub mod keccak;
pub mod types;
pub mod value;

pub use codec::CodecError;
pub use keccak::{keccak256, keccak256_bytes};
pub use geometry::{Backend, GeomError, Geometry, Sizes};
pub use types::{AddressHash, Balance, BlockDelta, BlockUpdate, CoalesceError, SegmentRowDeltas};
pub use value::{Lookup, ValueCodec, ValueError};
