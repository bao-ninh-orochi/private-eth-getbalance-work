//! `xtask web` — build the browser client's wasm and put it where the
//! server serves it from (ADR-0019).
//!
//! # Why a command rather than a line in the README
//!
//! The build needs a target that is not the host, a feature set that is
//! not the default (`--no-default-features`: no rayon, because
//! `wasm32-unknown-unknown` has no threads), and a `--release` profile
//! that is not optional (a debug wasm client is roughly an order of
//! magnitude slower — the difference between a lookup that feels instant
//! and one that does not). Getting any of those wrong produces something
//! that still *builds*, so it is worth having exactly one way to do it.
//!
//! The `getrandom_backend="custom"` flag the target also needs lives in
//! `.cargo/config.toml`, so a plain `cargo build --target
//! wasm32-unknown-unknown -p risepir-wasm` works too — this command adds
//! the profile, the feature flags, and the copy into `web/`.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Repo root, derived from this crate's manifest directory.
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().expect("canonicalize repo root")
}

/// Build `risepir-wasm` for wasm32 and copy the artifact to
/// `web/client.wasm`. Returns the byte size written.
///
/// # Panics
///
/// If the target is not installed, the build fails, or the artifact
/// cannot be copied — all of which are setup problems the operator has to
/// fix before anything can be served.
pub fn run() -> u64 {
    let root = repo_root();

    let status = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
        .current_dir(&root)
        .args([
            "build",
            "-p",
            "risepir-wasm",
            "--release",
            "--no-default-features",
            "--target",
            "wasm32-unknown-unknown",
        ])
        .status()
        .expect("run cargo build");

    if !status.success() {
        eprintln!();
        eprintln!("xtask web: the wasm build failed.");
        eprintln!("If the target is missing:  rustup target add wasm32-unknown-unknown");
        std::process::exit(1);
    }

    let artifact = root.join("target/wasm32-unknown-unknown/release/risepir_wasm.wasm");
    let dest = root.join("web/client.wasm");
    std::fs::copy(&artifact, &dest)
        .unwrap_or_else(|e| panic!("copy {} -> {}: {e}", artifact.display(), dest.display()));

    let size = std::fs::metadata(&dest).expect("stat client.wasm").len();
    println!("xtask web: wrote {} ({} bytes)", dest.display(), size);
    println!("xtask web: serve it with, e.g.:");
    println!("    risepir-rpc mock --web web");
    println!("    risepir-rpc mainnet --partial --partial-capacity 1000000 --web web");
    size
}
