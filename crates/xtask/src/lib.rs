#![forbid(unsafe_code)]
#![warn(missing_docs)]
//! `xtask`: repo-local developer tooling for the private `eth_getBalance`
//! workspace.
//!
//! Three things: the Stage 0.5 conformance harness (`docs/plan.md` §6
//! "Stages", §8 "never-return-a-wrong-answer checklist") — see
//! [`conformance::run`] — and the Stage 3 measured numbers table
//! (`docs/plan.md` §7, `docs/verification.md` §7) — see [`bench::run`] —
//! plus the browser client's wasm build ([`web::run`], ADR-0019).
//! Structured as a library (this crate) plus a thin binary (`src/main.rs`)
//! so both the CLI and the `tests/` integration tests drive the exact same
//! harness code — the binary is not a reimplementation of the tests, and
//! the tests are not a reimplementation of the binary.
//!
//! Also: the `geometry` subcommand ([`geometry`], ADR-0030) — an
//! `arity x bucket_size` sweep over `risepir_proto::geometry::Geometry`,
//! plus an opt-in real-store fill-check, for the recurring "should we
//! change the deployed SCF geometry" question.

pub mod bench;
pub mod conformance;
pub mod geometry;
pub mod web;
