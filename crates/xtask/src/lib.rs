#![forbid(unsafe_code)]
#![warn(missing_docs)]
//! `xtask`: repo-local developer tooling for the private `eth_getBalance`
//! workspace.
//!
//! Two things: the Stage 0.5 conformance harness (`docs/plan.md` §6
//! "Stages", §8 "never-return-a-wrong-answer checklist") — see
//! [`conformance::run`] — and the Stage 3 measured numbers table
//! (`docs/plan.md` §7, `docs/verification.md` §7) — see [`bench::run`].
//! Structured as a library (this crate) plus a thin binary (`src/main.rs`)
//! so both the CLI and the `tests/` integration tests drive the exact same
//! harness code — the binary is not a reimplementation of the tests, and
//! the tests are not a reimplementation of the binary.

pub mod bench;
pub mod conformance;
