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
//! truncating an overflowing balance; [`value::ValueCodec::decode`]
//! hard-fails on a checksum mismatch rather than returning a corrupted
//! number; and [`codec::decode_block_delta`] validates every count against
//! the input length before allocating, and every delta against the
//! plaintext modulus, so a malformed or hostile byte stream produces a
//! clean [`codec::CodecError`] instead of a panic, an OOM, or — worst of
//! all — a plausible-looking wrong value.

pub mod codec;
pub mod geometry;
pub mod types;
pub mod value;

pub use codec::CodecError;
pub use geometry::{Backend, GeomError, Geometry, Sizes};
pub use types::{AddressHash, Balance, BlockDelta, BlockUpdate, CoalesceError, SegmentRowDeltas};
pub use value::{ValueCodec, ValueError};
