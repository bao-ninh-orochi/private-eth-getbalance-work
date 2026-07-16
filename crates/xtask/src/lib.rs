#![forbid(unsafe_code)]
#![warn(missing_docs)]
//! `xtask`: repo-local developer tooling for the private `eth_getBalance`
//! workspace.
//!
//! Today this is exactly one thing: the Stage 0.5 conformance harness
//! (`docs/plan.md` §6 "Stages", §8 "never-return-a-wrong-answer checklist")
//! — see [`conformance::run`]. Structured as a library (this crate) plus a
//! thin binary (`src/main.rs`) so both the CLI and `tests/conformance.rs`
//! drive the exact same harness code — the binary is not a reimplementation
//! of the test, and the test is not a reimplementation of the binary.

pub mod conformance;
