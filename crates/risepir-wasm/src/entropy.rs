//! The browser client's entropy source — the single most
//! privacy-critical line in this crate.
//!
//! # Why this file exists
//!
//! Every PIR query's secrecy rests on the LWE secret `s` sampled inside
//! `sample_slot` (`ikpir-common`'s SimplePIR backend), which draws from
//! `rand::rng()` → `getrandom`. On `wasm32-unknown-unknown` there is no
//! ambient entropy: the target has no OS to ask. `getrandom` handles that
//! by *refusing to compile* unless a backend is named
//! (`--cfg getrandom_backend="…"`, set for this target in the workspace's
//! `.cargo/config.toml`).
//!
//! That refusal is a feature. A browser client that quietly fell back to a
//! fixed or predictable seed would still return correct balances — every
//! test would pass, the demo would look right — while the server could
//! reconstruct `s`, subtract `A·s` from the query, and read off exactly
//! which bucket was asked for. Silent entropy failure is the one bug in
//! this crate that breaks the property the whole project exists to
//! provide, and it is invisible from the outside. So it is made a build
//! error, and the shim below is the only thing that resolves it.
//!
//! # What the host must supply
//!
//! One import, `env.risepir_fill_random(ptr, len)`, which must fill
//! `len` bytes at `ptr` from a cryptographically secure source —
//! `crypto.getRandomValues` in a browser, `webcrypto.getRandomValues` in
//! Node. It is not optional: WebAssembly instantiation fails outright if
//! the import is missing, so "forgot to wire up the RNG" is a startup
//! crash, not a silent downgrade. `web/pir.js` is the reference host and
//! `web/test/e2e.mjs` asserts the shim is actually reached on every query.

// `risepir_fill_random(dest, len)` — fill `dest` with `len`
// cryptographically secure bytes. Provided by the host (see the module
// docs). Declared in the `env` module because that is where a plain
// `WebAssembly.instantiate` import object puts it: this crate deliberately
// does not use `wasm-bindgen`, so there is no generated glue hiding the
// contract.
#[cfg(target_arch = "wasm32")]
#[link(wasm_import_module = "env")]
extern "C" {
    fn risepir_fill_random(dest: *mut u8, len: usize);
}

/// `getrandom`'s `custom` backend hook, wired to the host CSPRNG.
///
/// # Safety
///
/// `getrandom` guarantees `dest` points to `len` writable bytes. The host
/// contract requires `risepir_fill_random` to write exactly that many and
/// no more. This is the only `unsafe` in the crate (`unsafe_code = "deny"`
/// at the crate level, with this one scoped exception) — everything else,
/// including the whole host ABI, moves bytes through buffers this module
/// owns, so no caller-supplied pointer is ever dereferenced.
#[cfg(target_arch = "wasm32")]
#[allow(unsafe_code)]
#[no_mangle]
unsafe extern "Rust" fn __getrandom_v03_custom(dest: *mut u8, len: usize) -> Result<(), getrandom::Error> {
    unsafe { risepir_fill_random(dest, len) };
    Ok(())
}

// On a native build (the `rlib` half of this crate — `cargo test
// --workspace` and the ABI tests) `getrandom` uses the OS backend as
// usual, and this shim is neither defined nor needed.
